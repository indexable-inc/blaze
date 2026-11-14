//! The tree-sitter corpus as the engine's fact source.
//!
//! `match` atoms are materialized once into rows (one row per tree-sitter query match, one
//! column per `@capture`) when the source is made; the engine joins them. Every value is a syntax
//! node or a piece of text derived from one, so the universe is finite and evaluation terminates.

use std::alloc::Global;
use std::borrow::Cow;
use std::ops::Range;

use ast_merge_langs::Lang;
use astlog_engine::{BodyAtom, Facts, InternalSnafu, Row};
use blaze_hash::HashMap;
use snafu::ResultExt as _;
use tree_sitter::StreamingIterator as _;

use crate::corpus::{Corpus, NodeRef, Value};
use crate::error::{
    CaptureAbsentSnafu, CaptureIndexSnafu, Error, NotNodeSnafu, PredicateUnsupportedSnafu,
    QuerySnafu,
};
use crate::program::{Atom, MatchAtom, Program, TreeSitter};

#[derive(Debug, PartialEq, Eq, Hash)]
struct MatchKey {
    lang: Lang,
    query: String,
}

/// The corpus with every `match` query of a program run over it.
pub(crate) struct TreeFacts<'a> {
    corpus: &'a Corpus,
    matches: HashMap<MatchKey, Vec<Row<NodeRef>>, Global>,
}

impl<'a> TreeFacts<'a> {
    /// Compile and run every distinct `match` query in the program.
    ///
    /// # Errors
    ///
    /// Fails on an invalid tree-sitter query, a query using `#` predicates, a capture index the
    /// compiled query does not know, or a match that leaves a capture out.
    pub(crate) fn new(program: &Program, corpus: &'a Corpus) -> Result<Self, Error> {
        let mut matches = HashMap::new_in(Global);
        let rule_atoms = program.rules.iter().flat_map(|rule| &rule.body);
        let rewrite_atoms = program.rewrites.iter().flat_map(|rewrite| &rewrite.body);
        for atom in rule_atoms.chain(rewrite_atoms) {
            // A negated `match` is asked for its rows the same way, to filter by them.
            let (BodyAtom::Source(source) | BodyAtom::NegatedSource(source)) = atom else {
                continue;
            };
            let Atom::Match(m) = &source.value else {
                continue;
            };
            let key = MatchKey { lang: m.lang, query: m.query.clone() };
            if let blaze_hash::hash_map::Entry::Vacant(entry) = matches.entry(key) {
                entry.insert(materialize(corpus, m, source.line)?);
            }
        }
        Ok(Self { corpus, matches })
    }

    /// Whether any named sibling attached above `node` has a descendant with this kind and exact
    /// text. Walks preceding siblings nearest-first and stops at the first sibling of the same
    /// kind as `node` (the previous definition/clause), so only the annotation block directly
    /// above `node` (comments and `@...` attributes) is searched. Bounded to that block, not the
    /// whole file.
    fn attached_has_descendant(&self, node: NodeRef, kind: &str, text: &str) -> bool {
        let file = &self.corpus.files[node.file];
        let Some(parent) = file.nodes[node.node].parent else {
            return false;
        };
        let node_kind = file.nodes[node.node].kind;
        // Preorder node table: a node's preceding siblings have lower indices, and everything
        // between `parent` and `node` is inside `parent`. Walk backward, considering only direct
        // children of `parent`.
        let mut index = node.node;
        while index > parent {
            index -= 1;
            let info = &file.nodes[index];
            if info.parent != Some(parent) {
                continue; // a descendant of an earlier sibling, skip
            }
            if info.kind == node_kind {
                break; // previous same-kind sibling: the annotation block ends here
            }
            if info.named
                && self.has_descendant(NodeRef { file: node.file, node: index }, kind, text)
            {
                return true;
            }
        }
        false
    }

    /// Whether `root` has a strict descendant with this kind and exact source text. Bounded to
    /// `root`'s subtree: in the preorder node table the strict descendants are the contiguous
    /// nodes after `root` whose start is within `root`'s byte span.
    fn has_descendant(&self, root: NodeRef, kind: &str, text: &str) -> bool {
        let file = &self.corpus.files[root.file];
        let root_end = file.nodes[root.node].end;
        let mut index = root.node + 1;
        while index < file.nodes.len() {
            let info = &file.nodes[index];
            if info.start >= root_end {
                break; // past root's subtree
            }
            if info.kind == kind && &file.text[info.start..info.end] == text {
                return true;
            }
            index += 1;
        }
        false
    }
}

impl Facts for TreeFacts<'_> {
    type Lang = TreeSitter;
    type Node = NodeRef;

    fn solve(
        &self,
        atom: &Atom,
        line: usize,
        inputs: &[Option<Value>],
        out: &mut Vec<Row<NodeRef>>,
    ) -> Result<(), Error> {
        match atom {
            Atom::Match(m) => {
                let key = MatchKey { lang: m.lang, query: m.query.clone() };
                let rows = self.matches.get(&key).ok_or_else(|| {
                    InternalSnafu {
                        what: format!("match query at line {line} was not materialized"),
                    }
                    .build()
                })?;
                out.extend(rows.iter().cloned());
            }
            Atom::NoDescendant | Atom::AttachedSibling => {
                let [Some(Value::Node(node)), Some(kind), Some(text)] = inputs else {
                    let what =
                        inputs.first().and_then(Option::as_ref).map_or("unbound", Value::what);
                    let name = if matches!(atom, Atom::NoDescendant) {
                        "no-descendant"
                    } else {
                        "attached-sibling"
                    };
                    return NotNodeSnafu { line, name, arg: "the first argument", what }
                        .fail()
                        .map_err(Into::into);
                };
                let kind = self.value_text(kind);
                let text = self.value_text(text);
                let holds = match atom {
                    Atom::NoDescendant => !self.has_descendant(*node, &kind, &text),
                    Atom::AttachedSibling | Atom::Match(_) => {
                        self.attached_has_descendant(*node, &kind, &text)
                    }
                };
                if holds {
                    out.push(inputs.iter().flatten().cloned().collect());
                }
            }
        }
        Ok(())
    }

    fn parent(&self, node: NodeRef) -> Option<NodeRef> {
        self.corpus.node_info(node).parent.map(|parent| NodeRef { file: node.file, node: parent })
    }

    fn kind(&self, node: NodeRef) -> Cow<'_, str> {
        Cow::Borrowed(self.corpus.node_info(node).kind)
    }

    fn text(&self, node: NodeRef) -> Cow<'_, str> {
        Cow::Borrowed(self.corpus.node_text(node))
    }

    fn span(&self, node: NodeRef) -> Option<(usize, Range<usize>)> {
        let info = self.corpus.node_info(node);
        Some((node.file, info.start..info.end))
    }

    fn path(&self, file: usize) -> Cow<'_, str> {
        self.corpus.files[file].path.to_string_lossy()
    }
}

/// Run one `match` query over every file of its language: one row per match, one column per
/// capture in [`MatchAtom::captures`] order.
fn materialize(corpus: &Corpus, m: &MatchAtom, line: usize) -> Result<Vec<Row<NodeRef>>, Error> {
    if has_predicate(&m.query) {
        return PredicateUnsupportedSnafu { line }.fail().map_err(Into::into);
    }
    let language = m.lang.to_tree_sitter();
    let query = tree_sitter::Query::new(&language, &m.query).context(QuerySnafu { line })?;
    let names = query.capture_names();
    let mut rows = Vec::new();
    for (file_index, file) in corpus.files.iter().enumerate() {
        if file.lang != m.lang {
            continue;
        }
        let mut cursor = tree_sitter::QueryCursor::new();
        let mut matches = cursor.matches(&query, file.root(), file.text.as_bytes());
        while let Some(found) = matches.next() {
            let mut row: Vec<Option<Value>> = vec![None; m.captures.len()];
            for capture in found.captures {
                let Ok(index) = usize::try_from(capture.index) else {
                    return CaptureIndexSnafu { line, index: capture.index }
                        .fail()
                        .map_err(Into::into);
                };
                let Some(name) = names.get(index) else {
                    return CaptureIndexSnafu { line, index: capture.index }
                        .fail()
                        .map_err(Into::into);
                };
                let Some(column) = m.captures.iter().position(|c| c == name) else {
                    return InternalSnafu {
                        what: format!("capture `{name}` at rules:{line} is not a column"),
                    }
                    .fail();
                };
                // A quantified capture contributes its first node per match.
                if row[column].is_some() {
                    continue;
                }
                let node = file.node_index(capture.node.id()).ok_or_else(|| {
                    InternalSnafu {
                        what: format!("query capture `{name}` missing from the node table"),
                    }
                    .build()
                })?;
                row[column] = Some(Value::Node(NodeRef { file: file_index, node }));
            }
            let row = row
                .into_iter()
                .zip(&m.captures)
                .map(|(value, capture)| {
                    value.ok_or_else(|| {
                        Error::from(
                            CaptureAbsentSnafu {
                                line,
                                capture: capture.clone(),
                                path: file.path.clone(),
                            }
                            .build(),
                        )
                    })
                })
                .collect::<Result<Row<NodeRef>, Error>>()?;
            rows.push(row);
        }
    }
    Ok(rows)
}

/// Detect `#` predicate syntax outside string literals and `;` comments (tree-sitter query
/// comments run to end of line).
fn has_predicate(query: &str) -> bool {
    let mut in_string = false;
    let mut chars = query.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => in_string = !in_string,
            '\\' if in_string => {
                chars.next();
            }
            ';' if !in_string => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            '#' if !in_string => return true,
            _ => {}
        }
    }
    false
}
