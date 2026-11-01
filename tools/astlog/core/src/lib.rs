//! Datalog over tree-sitter syntax trees.
//!
//! Tree-sitter query matches become relations (one row per match, columns
//! named by `@capture`), Datalog rules join them — structurally (`ancestor`,
//! `parent`, `same-file`), by value (`text`, `same-text`, `kind`), or
//! recursively — rewrites turn derived rows into byte-range edits built from
//! templates, and `(lint ...)` declarations turn derived rows into located
//! findings filtered by `astlog-ignore` suppression comments. See
//! `tools/astlog/README.md` for the language and the prior art this
//! composes.
//!
//! ```text
//! (rule (unwrap-call call e)
//!   (match rust "
//!     (call_expression
//!       function: (field_expression value: (_) @e field: (field_identifier) @m)
//!       arguments: (arguments)) @call")
//!   (text m "unwrap"))
//!
//! (rule (result-fn f)
//!   (match rust "(function_item return_type: (generic_type type: (type_identifier) @r)) @f")
//!   (text r "Result"))
//!
//! (rule (fixable call e)
//!   (unwrap-call call e)
//!   (result-fn f)
//!   (ancestor f call))
//!
//! (rewrite unwrap-to-try (fixable call e)
//!   (replace call "{e}?"))
//! ```

mod corpus;
mod error;
mod eval;
mod program;
mod rewrite;
mod scan;
mod sexpr;

#[cfg(test)]
mod tests;

use std::path::PathBuf;

pub use ast_merge_langs::Lang;

pub use crate::corpus::{Corpus, LineCol, NodeInfo, NodeRef, SourceFile, Value};
pub use crate::error::Error;
pub use crate::eval::{Database, Relation, Row};
pub use crate::program::{Lint, Program, Severity};
pub use crate::rewrite::{Edit, FileRewrite};
pub use crate::scan::{Finding, SuppressedFinding, one_line};

/// A finished run: the checked program, the loaded corpus, every derived
/// relation, and the edits every `(rewrite ...)` produced.
#[derive(Debug)]
pub struct Analysis {
    pub program: Program,
    pub corpus: Corpus,
    pub database: Database,
    pub edits: Vec<Edit>,
}

impl Analysis {
    /// Contents of each file after applying all edits (changed files only).
    #[must_use]
    pub fn rewritten(&self) -> Vec<FileRewrite> {
        rewrite::apply(&self.corpus, &self.edits)
    }

    /// Unified diff of all pending rewrites.
    #[must_use]
    pub fn diff(&self) -> String {
        rewrite::unified_diff(&self.corpus, &self.rewritten())
    }

    /// Findings from every `(lint ...)` declaration, with `astlog-ignore`
    /// suppression applied and sorted by (file, line, column, rule).
    ///
    /// # Errors
    ///
    /// Fails when a lint relation derives a row with no node-valued column
    /// to locate the finding at.
    pub fn findings(&self) -> Result<Vec<Finding>, Error> {
        scan::findings(&self.program, &self.corpus, &self.database)
    }

    /// Findings an `astlog-ignore` comment suppressed, each paired with the
    /// comment that suppressed it, sorted like [`Analysis::findings`]. This is
    /// the audit view: what is being explicitly ignored, where, and why.
    ///
    /// # Errors
    ///
    /// Fails when a lint relation derives a row with no node-valued column
    /// to locate the finding at.
    pub fn suppressed(&self) -> Result<Vec<SuppressedFinding>, Error> {
        scan::suppressed(&self.program, &self.corpus, &self.database)
    }
}

/// Parse `rules`, load `paths`, run rules to a fixpoint, and plan rewrites.
///
/// # Errors
///
/// Fails on a malformed rules file, an unloadable or unparseable source path,
/// an invalid tree-sitter query, an unbound variable at evaluation, or
/// overlapping rewrites.
pub fn analyze(rules: &str, paths: &[PathBuf]) -> Result<Analysis, Error> {
    let program = Program::parse(rules)?;
    let corpus = Corpus::load(paths)?;
    let evaluator = eval::Evaluator::new(&program, &corpus)?;
    let database = evaluator.fixpoint()?;
    let edits = rewrite::collect(&program, &corpus, &evaluator, &database)?;
    drop(evaluator);
    Ok(Analysis { program, corpus, database, edits })
}
