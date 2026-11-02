//! Source files parsed into queryable trees plus a flat node table.
//!
//! The node table (preorder, with parent indices) is what the Datalog builtins
//! walk: `parent`/`ancestor` follow indices instead of re-traversing trees,
//! and a [`NodeRef`] stays `Copy + Eq + Hash + Ord` so it can live in relation
//! rows.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ast_merge_langs::Lang;
use blaze_hash::HashMap;
use snafu::ResultExt as _;

use crate::error::{
    Error, LanguageSnafu, ParseFileSnafu, ReadFileSnafu, UnknownLanguageSnafu, WalkSnafu,
};

/// One value in a relation row: a syntax node or derived text.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Value {
    Node(NodeRef),
    Text(Arc<str>),
}

/// A node identified by corpus file index and node-table index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeRef {
    pub file: usize,
    pub node: usize,
}

#[derive(Debug)]
pub struct NodeInfo {
    pub kind: &'static str,
    pub start: usize,
    pub end: usize,
    pub parent: Option<usize>,
    /// Tree-sitter "named" node (a grammar rule, not an anonymous token like
    /// `do` or `,`). Sibling-walking builtins skip anonymous nodes so they see
    /// the syntactic statements, not the punctuation between them.
    pub named: bool,
}

/// 1-based line/column of a byte offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineCol {
    pub line: usize,
    pub column: usize,
}

#[derive(Debug)]
pub struct SourceFile {
    pub path: PathBuf,
    pub lang: Lang,
    pub text: String,
    pub nodes: Vec<NodeInfo>,
    line_starts: Vec<usize>,
    by_id: HashMap<usize, usize>,
    tree: tree_sitter::Tree,
}

impl SourceFile {
    pub(crate) fn root(&self) -> tree_sitter::Node<'_> {
        self.tree.root_node()
    }

    pub(crate) fn node_index(&self, id: usize) -> Option<usize> {
        self.by_id.get(&id).copied()
    }
}

#[derive(Debug)]
pub struct Corpus {
    pub files: Vec<SourceFile>,
}

impl Corpus {
    /// Load `paths` into parsed trees. Directories are walked recursively
    /// (gitignore-aware via [`ignore`]) and files without a known grammar are
    /// skipped; an explicitly listed file must have a known grammar.
    ///
    /// # Errors
    ///
    /// Fails on unreadable paths, walk errors, an explicitly listed file with
    /// no registered grammar, or a tree-sitter parser refusing a file.
    pub fn load(paths: &[PathBuf]) -> Result<Self, Error> {
        let mut sources: Vec<(PathBuf, Lang)> = Vec::new();
        for path in paths {
            if path.is_dir() {
                collect_dir(path, &mut sources)?;
            } else {
                let lang = ast_merge_langs::detect(path)
                    .ok_or_else(|| UnknownLanguageSnafu { path: path.clone() }.build())?;
                sources.push((path.clone(), lang));
            }
        }
        sources.sort_by(|(a, _), (b, _)| a.cmp(b));
        sources.dedup_by(|(a, _), (b, _)| a == b);

        let mut parser = tree_sitter::Parser::new();
        let files = sources
            .into_iter()
            .map(|(path, lang)| load_file(&mut parser, path, lang))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { files })
    }

    #[must_use]
    pub fn node_info(&self, node: NodeRef) -> &NodeInfo {
        &self.files[node.file].nodes[node.node]
    }

    #[must_use]
    pub fn node_text(&self, node: NodeRef) -> &str {
        let file = &self.files[node.file];
        let info = &file.nodes[node.node];
        &file.text[info.start..info.end]
    }

    /// Text behind any value: a node's source slice or the text itself.
    #[must_use]
    pub fn value_text<'a>(&'a self, value: &'a Value) -> &'a str {
        match value {
            Value::Node(node) => self.node_text(*node),
            Value::Text(text) => text,
        }
    }

    #[must_use]
    pub fn position(&self, file: usize, byte: usize) -> LineCol {
        let line_starts = &self.files[file].line_starts;
        let line = line_starts.partition_point(|&start| start <= byte);
        LineCol { line, column: byte - line_starts[line - 1] + 1 }
    }
}

fn collect_dir(path: &Path, sources: &mut Vec<(PathBuf, Lang)>) -> Result<(), Error> {
    for entry in ignore::Walk::new(path) {
        let entry = entry.context(WalkSnafu { path })?;
        let is_file = entry.file_type().is_some_and(|ty| ty.is_file());
        if !is_file {
            continue;
        }
        if let Some(lang) = ast_merge_langs::detect(entry.path()) {
            sources.push((entry.into_path(), lang));
        }
    }
    Ok(())
}

fn load_file(
    parser: &mut tree_sitter::Parser,
    path: PathBuf,
    lang: Lang,
) -> Result<SourceFile, Error> {
    let text = std::fs::read_to_string(&path).context(ReadFileSnafu { path: &path })?;
    parser.set_language(&lang.to_tree_sitter()).context(LanguageSnafu { path: &path })?;
    let tree = parser.parse(&text, None).ok_or_else(|| ParseFileSnafu { path: &path }.build())?;
    let table = build_node_table(&tree);
    let line_starts = build_line_starts(&text);
    Ok(SourceFile { path, lang, text, nodes: table.nodes, line_starts, by_id: table.by_id, tree })
}

/// Preorder node table with parent indices, plus tree-sitter node id lookup.
struct NodeTable {
    nodes: Vec<NodeInfo>,
    by_id: HashMap<usize, usize>,
}

fn build_node_table(tree: &tree_sitter::Tree) -> NodeTable {
    let mut nodes = Vec::new();
    let mut by_id = HashMap::default();
    let mut parents: Vec<usize> = Vec::new();
    let mut cursor = tree.walk();
    loop {
        let node = cursor.node();
        let index = nodes.len();
        by_id.insert(node.id(), index);
        nodes.push(NodeInfo {
            kind: node.kind(),
            start: node.start_byte(),
            end: node.end_byte(),
            parent: parents.last().copied(),
            named: node.is_named(),
        });
        if cursor.goto_first_child() {
            parents.push(index);
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return NodeTable { nodes, by_id };
            }
            parents.pop();
        }
    }
}

fn build_line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(text.char_indices().filter_map(
        |(at, c)| {
            if c == '\n' { Some(at + 1) } else { None }
        },
    ));
    starts
}
