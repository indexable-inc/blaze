//! This module defines Concrete Syntax Tree (CST), used by rust-analyzer.
//!
//! The CST includes comments and whitespace, provides a single node type,
//! `SyntaxNode`, and a basic traversal API (parent, children, siblings).
//!
//! The *real* implementation is in the (language-agnostic) `syntax-tree` crate, this
//! module just wraps its API.

use std::sync::Arc;

use syntax_tree::{FlatTree, FlatTreeBuilder, Language, Text};

use crate::{Parse, SyntaxError, SyntaxKind, TextRange, TextSize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RustLanguage {}
impl Language for RustLanguage {
    type Kind = SyntaxKind;

    fn kind_from_raw(raw: syntax_tree::SyntaxKind) -> SyntaxKind {
        SyntaxKind::from(raw.0)
    }

    fn kind_to_raw(kind: SyntaxKind) -> syntax_tree::SyntaxKind {
        syntax_tree::SyntaxKind(kind.into())
    }
}

pub type SyntaxNode = syntax_tree::SyntaxNode<RustLanguage>;
pub type SyntaxToken = syntax_tree::SyntaxToken<RustLanguage>;
pub type SyntaxElement = syntax_tree::SyntaxElement<RustLanguage>;
pub type SyntaxNodeChildren = syntax_tree::SyntaxNodeChildren<RustLanguage>;
pub type SyntaxElementChildren = syntax_tree::SyntaxElementChildren<RustLanguage>;
pub type PreorderWithTokens = syntax_tree::api::PreorderWithTokens<RustLanguage>;

/// The tree of a parse, flat: every tree is.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Tree(Arc<FlatTree>);

impl Tree {
    pub fn new(tree: Arc<FlatTree>) -> Tree {
        Tree(tree)
    }

    pub fn root(&self) -> SyntaxNode {
        SyntaxNode::new_flat_root(Arc::clone(&self.0)) // clone: a handle root's count; goes with the handles
    }

    pub fn flat(&self) -> &Arc<FlatTree> {
        &self.0
    }

    /// The tree, when this is the last count of it.
    pub(crate) fn into_unique(self) -> Option<FlatTree> {
        Arc::into_inner(self.0)
    }
}

pub struct SyntaxTreeBuilder {
    errors: Vec<SyntaxError>,
    inner: FlatTreeBuilder,
    lazy: bool,
}

impl Default for SyntaxTreeBuilder {
    /// A builder that keeps the text its tokens are given, for trees not parsed from one text
    /// (macro expansions).
    fn default() -> SyntaxTreeBuilder {
        // alloc: `Vec::new` allocates nothing until an error is pushed
        SyntaxTreeBuilder { errors: Vec::new(), inner: FlatTreeBuilder::new(), lazy: false }
    }
}

impl SyntaxTreeBuilder {
    /// A builder whose tokens are consecutive slices of `range` of `source`, which the tree shares.
    pub(crate) fn sourced_range(source: Text, range: TextRange) -> SyntaxTreeBuilder {
        SyntaxTreeBuilder {
            errors: Vec::new(), // alloc: `Vec::new` allocates nothing until an error is pushed
            inner: FlatTreeBuilder::sourced_range(source, range),
            lazy: false,
        }
    }

    /// The tree and the errors given; a tree with lazy nodes expands them with
    /// [`crate::parsing::expand_lazy`] in `edition`.
    pub(crate) fn finish_raw(mut self, edition: parser::Edition) -> (FlatTree, Vec<SyntaxError>) {
        if self.lazy {
            self.inner.expand_with(crate::parsing::expand_lazy, edition as u32);
        }
        (self.inner.finish(), self.errors)
    }

    pub fn finish(self) -> Parse<SyntaxNode> {
        self.finish_as()
    }

    /// [`SyntaxTreeBuilder::finish`] tagged [`crate::view::Untyped`], read through views.
    pub fn finish_view(self) -> Parse<crate::view::Untyped> {
        self.finish_as()
    }

    fn finish_as<T>(self) -> Parse<T> {
        let (tree, errors) = self.finish_raw(parser::Edition::CURRENT);
        let tree = Tree(Arc::new(tree)); // alloc: the tree's one shared box, once per parse
        // Disable block validation, see https://github.com/rust-lang/rust-analyzer/pull/10357
        #[allow(clippy::overly_complex_bool_expr)]
        if cfg!(debug_assertions) && false {
            crate::validation::validate_block_structure(crate::view::Node::root(tree.flat()));
        }
        Parse::new(tree, errors)
    }

    pub fn token(&mut self, kind: SyntaxKind, text: &str) {
        self.inner.token(RustLanguage::kind_to_raw(kind), text);
    }

    pub fn start_node(&mut self, kind: SyntaxKind) {
        self.inner.start_node(RustLanguage::kind_to_raw(kind));
    }

    /// The source's next `len` bytes as one lazy node of `kind`, for a sourced builder.
    pub(crate) fn lazy(&mut self, kind: SyntaxKind, len: TextSize) {
        self.lazy = true;
        self.inner.lazy(RustLanguage::kind_to_raw(kind), len.into());
    }

    pub fn finish_node(&mut self) {
        self.inner.finish_node();
    }

    pub fn error(&mut self, error: String, text_pos: TextSize) {
        self.errors.push(SyntaxError::new_at_offset(error, text_pos));
    }
}
