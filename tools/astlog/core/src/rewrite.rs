//! Rewrites: template instantiation over rewrite-body bindings into byte-range
//! edits. The apply / diff / overlap mechanics live in the shared `edit-applier`
//! crate; this module only turns derived bindings
//! into [`Edit`]s and adapts the tree-sitter [`Corpus`] to the applier's
//! `(path, contents)` view.

use std::path::PathBuf;

use blaze_hash::HashMap;

use crate::corpus::{Corpus, Value};
use crate::error::{Error, OverlappingEditsSnafu, ReplaceNotNodeSnafu, TemplateVarSnafu};
use crate::eval::{Database, Evaluator};
use crate::program::{Program, Rewrite, Segment};

pub use edit_applier::{Edit, FileRewrite};

pub(crate) fn collect(
    program: &Program,
    corpus: &Corpus,
    evaluator: &Evaluator<'_>,
    db: &Database,
) -> Result<Vec<Edit>, Error> {
    let mut edits = Vec::new();
    for rewrite in &program.rewrites {
        for env in evaluator.bindings(db, &rewrite.body)? {
            edits.push(edit_of(corpus, rewrite, &env)?);
        }
    }
    edits.sort();
    edits.dedup();
    let paths: Vec<PathBuf> = corpus.files.iter().map(|file| file.path.clone()).collect();
    edit_applier::check_overlaps(&paths, &edits).map_err(|error| {
        OverlappingEditsSnafu {
            path: error.path,
            first_start: error.first_start,
            first_end: error.first_end,
            second_start: error.second_start,
            second_end: error.second_end,
        }
        .build()
    })?;
    Ok(edits)
}

fn edit_of(
    corpus: &Corpus,
    rewrite: &Rewrite,
    env: &HashMap<String, Value>,
) -> Result<Edit, Error> {
    let target = env.get(&rewrite.target).ok_or_else(|| {
        TemplateVarSnafu { var: rewrite.target.clone(), line: rewrite.line }.build()
    })?;
    let Value::Node(node) = target else {
        return ReplaceNotNodeSnafu { name: rewrite.name.clone(), var: rewrite.target.clone() }
            .fail();
    };
    let mut replacement = String::new();
    for segment in &rewrite.template.segments {
        match segment {
            Segment::Lit(lit) => replacement.push_str(lit),
            Segment::Var(var) => {
                let value = env.get(var).ok_or_else(|| {
                    TemplateVarSnafu { var: var.clone(), line: rewrite.template.line }.build()
                })?;
                replacement.push_str(corpus.value_text(value));
            }
        }
    }
    let info = corpus.node_info(*node);
    Ok(Edit { file: node.file, start: info.start, end: info.end, replacement })
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
