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

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

pub use ast_merge_langs::Lang;
use astlog_engine::{Base, Includes};
pub use astlog_engine::{Lint, Severity};

pub use crate::corpus::{Corpus, LineCol, NodeInfo, NodeRef, SourceFile, Value};
pub use crate::error::{Error, SourceError};
pub use crate::program::{Atom, MatchAtom, Program, TreeSitter};
pub use crate::rewrite::{Edit, FileRewrite};
pub use crate::scan::{Finding, SuppressedFinding, one_line};

/// Every relation the program derived over the corpus.
pub type Database = astlog_engine::Database<NodeRef>;
pub type Relation = astlog_engine::Relation<NodeRef>;
pub type Row = astlog_engine::Row<NodeRef>;

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
/// An `(include ...)` in `rules` is refused: the text has no file to read one beside
/// ([`analyze_at`] has).
pub fn analyze(rules: &str, paths: &[PathBuf]) -> Result<Analysis, Error> {
    run(Program::parse(rules)?, paths)
}

/// [`analyze`] of `rules`, the text of the rules file at `at`: an `(include "path")` reads the
/// file at `path` relative to the directory of the file it is written in, from disk.
///
/// # Errors
///
/// As [`analyze`], and an include that names no file, closes a cycle or passes a cap.
pub fn analyze_at(rules: &str, at: &Path, paths: &[PathBuf]) -> Result<Analysis, Error> {
    run(Program::load(rules, &Disk { root: at.to_str() })?, paths)
}

/// Rules files on disk, each include read beside the file that names it. A rules file whose
/// path is not UTF-8 has no key to read beside, so its includes are refused.
struct Disk<'a> {
    root: Option<&'a str>,
}

impl Includes for Disk<'_> {
    fn base(&self) -> Base<'_> {
        Base::Beside(self.root)
    }

    fn read<R>(&self, key: &str, with: impl FnOnce(&str) -> R) -> std::io::Result<Option<R>> {
        match std::fs::read_to_string(key) {
            Ok(text) => Ok(Some(with(&text))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
}

fn run(program: Program, paths: &[PathBuf]) -> Result<Analysis, Error> {
    let corpus = Corpus::load(paths)?;
    let facts = eval::TreeFacts::new(&program, &corpus)?;
    let (database, planned) = astlog_engine::run(&program, &facts)?;
    let edits = rewrite::collect(&corpus, &planned)?;
    drop(planned);
    drop(facts);
    Ok(Analysis { program, corpus, database, edits })
}
