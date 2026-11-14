//! Rewrites: the engine's planned text actions as byte-range edits. The apply / diff / overlap
//! mechanics live in the shared `edit-applier` crate; this module only places each action at its
//! node and adapts the tree-sitter [`Corpus`] to the applier's `(path, contents)` view.

use std::path::PathBuf;

use astlog_engine::{Planned, PlannedAction, TextKind};

use crate::corpus::{Corpus, NodeRef};
use crate::error::{Error, OverlappingEditsSnafu};
use crate::program::Action;

pub use edit_applier::{Edit, FileRewrite};

pub(crate) fn collect(
    corpus: &Corpus,
    planned: &[Planned<'_, NodeRef, Action>],
) -> Result<Vec<Edit>, Error> {
    let mut edits = Vec::new();
    for plan in planned {
        match &plan.action {
            PlannedAction::Text { kind, node, text, .. } => {
                let info = corpus.node_info(*node);
                let (start, end) = match kind {
                    TextKind::Replace | TextKind::Delete => (info.start, info.end),
                    TextKind::InsertBefore => (info.start, info.start),
                    TextKind::InsertAfter => (info.end, info.end),
                };
                edits.push(Edit { file: node.file, start, end, replacement: text.clone() });
            }
            PlannedAction::Source { value, .. } => match **value {},
        }
    }
    edits.sort();
    edits.dedup();
    let paths: Vec<PathBuf> = corpus.files.iter().map(|file| file.path.clone()).collect();
    edit_applier::check_overlaps(&paths, &edits).map_err(|error| {
        Error::from(
            OverlappingEditsSnafu {
                path: error.path,
                first_start: error.first_start,
                first_end: error.first_end,
                second_start: error.second_start,
                second_end: error.second_end,
            }
            .build(),
        )
    })?;
    Ok(edits)
}

/// `(path, contents)` for every corpus file, the view `edit-applier` consumes.
fn corpus_files(corpus: &Corpus) -> Vec<edit_applier::Source> {
    corpus.files.iter().map(|file| (file.path.clone(), file.text.clone())).collect()
}

/// Apply edits, returning only the files that changed.
#[must_use]
pub(crate) fn apply(corpus: &Corpus, edits: &[Edit]) -> Vec<FileRewrite> {
    edit_applier::apply(&corpus_files(corpus), edits)
}

/// Unified diff of every pending rewrite against the loaded corpus.
#[must_use]
pub(crate) fn unified_diff(corpus: &Corpus, rewrites: &[FileRewrite]) -> String {
    edit_applier::unified_diff(&corpus_files(corpus), rewrites)
}
