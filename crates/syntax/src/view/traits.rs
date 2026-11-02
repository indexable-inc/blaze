//! The traits nodes share, on views; implemented in `generated/nodes.rs`.

use crate::{
    SyntaxKind, T, match_ast,
    view::{self as ast, AstChildren, AstNode, Node, Token, support},
};

pub trait HasName<'t>: AstNode<'t> {
    fn name(&self) -> Option<ast::Name<'t>> {
        support::child(self.syntax())
    }
}

pub trait HasVisibility<'t>: AstNode<'t> {
    fn visibility(&self) -> Option<ast::Visibility<'t>> {
        support::child(self.syntax())
    }
}

pub trait HasLoopBody<'t>: AstNode<'t> {
    fn loop_body(&self) -> Option<ast::BlockExpr<'t>> {
        support::child(self.syntax())
    }

    fn label(&self) -> Option<ast::Label<'t>> {
        support::child(self.syntax())
    }
}

pub trait HasArgList<'t>: AstNode<'t> {
    fn arg_list(&self) -> Option<ast::ArgList<'t>> {
        support::child(self.syntax())
    }
}

pub trait HasModuleItem<'t>: AstNode<'t> {
    fn items(&self) -> AstChildren<'t, ast::Item<'t>> {
        support::children(self.syntax())
    }
}

pub trait HasGenericParams<'t>: AstNode<'t> {
    fn generic_param_list(&self) -> Option<ast::GenericParamList<'t>> {
        support::child(self.syntax())
    }

    fn where_clause(&self) -> Option<ast::WhereClause<'t>> {
        support::child(self.syntax())
    }
}

pub trait HasGenericArgs<'t>: AstNode<'t> {
    fn generic_arg_list(&self) -> Option<ast::GenericArgList<'t>> {
        support::child(self.syntax())
    }
}

pub trait HasTypeBounds<'t>: AstNode<'t> {
    fn type_bound_list(&self) -> Option<ast::TypeBoundList<'t>> {
        support::child(self.syntax())
    }

    fn colon_token(&self) -> Option<Token<'t>> {
        support::token(self.syntax(), T![:])
    }
}

pub trait HasAttrs<'t>: AstNode<'t> {
    fn attrs(&self) -> AstChildren<'t, ast::Attr<'t>> {
        support::children(self.syntax())
    }

    fn doc_comments(&self) -> AstChildren<'t, ast::DocComment<'t>> {
        support::children(self.syntax())
    }

    fn attrs_with_doc(&self) -> AstChildren<'t, ast::AnyAttr<'t>> {
        support::children(self.syntax())
    }

    /// The node holding this node's inner attributes, which may be this node itself (for a
    /// `SourceFile`): the caller avoids taking an attribute twice.
    fn inner_attributes_node(&self) -> Option<Node<'t>> {
        let syntax = self.syntax();
        Some(match_ast! {
            match syntax {
                // A `SourceFile` contains the inner attributes of itself.
                ast::SourceFile(_) => syntax,
                ast::ExternBlock(it) => it.extern_item_list()?.syntax(),
                ast::Fn(it) => {
                    let body = it.body()?;
                    // An outline's lazy body is parsed when a walk enters it: one that surely
                    // starts with no inner attribute stays unparsed.
                    let lazy = body.syntax().is_lazy();
                    if lazy && !block_may_start_with_inner_attr(body.syntax().text(), false) {
                        return None;
                    }
                    body.stmt_list()?.syntax()
                },
                ast::MatchExpr(it) => it.match_arm_list()?.syntax(),
                ast::Impl(it) => it.assoc_item_list()?.syntax(),
                ast::Trait(it) => it.assoc_item_list()?.syntax(),
                ast::Module(it) => it.item_list()?.syntax(),
                // Any block expression's, as rustc reads them (`may_carry_attributes` is where
                // outer ones may go), but a body's, which are its function's. The parser puts
                // them in the block's statement list.
                ast::BlockExpr(it) => {
                    if syntax.parent().is_some_and(|it| it.kind() == SyntaxKind::FN) {
                        return None;
                    }
                    it.stmt_list()?.syntax()
                },
                _ => return None,
            }
        })
    }
}

/// Whether the text of a block, its `{` included, may start with an inner attribute (or, when
/// `inner_docs`, an inner doc comment) after whitespace and comments, as the parser's
/// `inner_attrs` takes them: false only when it surely does not. A non-ASCII character, an
/// unterminated comment or a text that is not a block is a maybe.
pub fn block_may_start_with_inner_attr(block: &str, inner_docs: bool) -> bool {
    let Some(mut rest) = block.strip_prefix('{') else { return true };
    loop {
        rest = rest
            .trim_start_matches(|c: char| matches!(c, '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' '));
        if let Some(comment) = rest.strip_prefix("//") {
            if inner_docs && comment.starts_with('!') {
                return true;
            }
            match comment.find('\n') {
                Some(end) => rest = &comment[end..],
                None => return true,
            }
        } else if let Some(comment) = rest.strip_prefix("/*") {
            if inner_docs && comment.starts_with('!') {
                return true;
            }
            match block_comment_len(comment) {
                Some(end) => rest = &comment[end..],
                None => return true,
            }
        } else {
            return rest.chars().next().is_none_or(|c| c == '#' || !c.is_ascii());
        }
    }
}

/// How long the rest of a block comment is, after its opening `/*`, to just past the `*/` that
/// closes it, nested comments included as the lexer counts them.
fn block_comment_len(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let (mut depth, mut at) = (1usize, 0);
    while at + 1 < bytes.len() {
        match (bytes[at], bytes[at + 1]) {
            (b'/', b'*') => {
                depth += 1;
                at += 2;
            }
            (b'*', b'/') => {
                depth -= 1;
                at += 2;
                if depth == 0 {
                    return Some(at);
                }
            }
            _ => at += 1,
        }
    }
    None
}

/// Every attribute of `owner` with its doc comments, the inner ones under a child included.
pub fn attrs_with_doc_including_inner<'t, O: HasAttrs<'t>>(
    owner: &O,
) -> impl Iterator<Item = ast::AnyAttr<'t>> + Clone + use<'t, O> {
    owner.attrs_with_doc().filter(|attr| attr.kind().is_outer()).chain(
        owner
            .inner_attributes_node()
            .into_iter()
            .flat_map(support::children::<ast::AnyAttr<'t>>)
            .filter(|attr| attr.kind().is_inner()),
    )
}

/// Every attribute of `owner`, the inner ones under a child included.
pub fn attrs_including_inner<'t, O: HasAttrs<'t>>(
    owner: &O,
) -> impl Iterator<Item = ast::Attr<'t>> + Clone + use<'t, O> {
    AttrsIter::new(attrs_with_doc_including_inner(owner))
}

#[derive(Clone)]
pub struct AttrsIter<I> {
    inner: I,
}

impl<I> AttrsIter<I> {
    #[inline]
    pub fn new(inner: I) -> Self {
        Self { inner }
    }
}

impl<'t, I: Iterator<Item = ast::AnyAttr<'t>>> Iterator for AttrsIter<I> {
    type Item = ast::Attr<'t>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.find_map(|attr| match attr {
            ast::AnyAttr::Attr(it) => Some(it),
            ast::AnyAttr::DocComment(_) => None,
        })
    }
}
