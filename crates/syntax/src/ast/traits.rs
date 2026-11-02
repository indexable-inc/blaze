//! Various traits that are implemented by ast nodes.
//!
//! The implementations are usually trivial, and live in generated.rs
use either::Either;

use crate::{
    SyntaxKind, SyntaxNode, SyntaxToken, T,
    ast::{self, AstChildren, AstNode, support},
    match_ast,
};

pub trait HasName: AstNode {
    fn name(&self) -> Option<ast::Name> {
        support::child(self.syntax())
    }
}

pub trait HasVisibility: AstNode {
    fn visibility(&self) -> Option<ast::Visibility> {
        support::child(self.syntax())
    }
}

pub trait HasLoopBody: AstNode {
    fn loop_body(&self) -> Option<ast::BlockExpr> {
        support::child(self.syntax())
    }

    fn label(&self) -> Option<ast::Label> {
        support::child(self.syntax())
    }
}

pub trait HasArgList: AstNode {
    fn arg_list(&self) -> Option<ast::ArgList> {
        support::child(self.syntax())
    }
}

pub trait HasModuleItem: AstNode {
    fn items(&self) -> AstChildren<ast::Item> {
        support::children(self.syntax())
    }
}

pub trait HasGenericParams: AstNode {
    fn generic_param_list(&self) -> Option<ast::GenericParamList> {
        support::child(self.syntax())
    }

    fn where_clause(&self) -> Option<ast::WhereClause> {
        support::child(self.syntax())
    }
}
pub trait HasGenericArgs: AstNode {
    fn generic_arg_list(&self) -> Option<ast::GenericArgList> {
        support::child(self.syntax())
    }
}

pub trait HasTypeBounds: AstNode {
    fn type_bound_list(&self) -> Option<ast::TypeBoundList> {
        support::child(self.syntax())
    }

    fn colon_token(&self) -> Option<SyntaxToken> {
        support::token(self.syntax(), T![:])
    }
}

pub trait HasAttrs: AstNode {
    fn attrs(&self) -> AstChildren<ast::Attr> {
        support::children(self.syntax())
    }

    fn doc_comments(&self) -> AstChildren<ast::DocComment> {
        support::children(self.syntax())
    }

    fn attrs_with_doc(&self) -> AstChildren<ast::AnyAttr> {
        support::children(self.syntax())
    }

    /// This may return the same node as called with (with `SourceFile`). The caller has the responsibility
    /// to avoid duplicate attributes.
    fn inner_attributes_node(&self) -> Option<SyntaxNode> {
        let syntax = self.syntax();
        Some(match_ast! {
            match syntax {
                // A `SourceFile` contains the inner attributes of itself.
                ast::SourceFile(_) => syntax.clone(),
                ast::ExternBlock(it) => it.extern_item_list()?.syntax().clone(),
                ast::Fn(it) => {
                    let body = it.body()?;
                    // An outline's lazy body is parsed when a cursor enters it: one that surely
                    // starts with no inner attribute stays unparsed.
                    if body.syntax().is_lazy() && !may_start_with_inner_attr(body.syntax()) {
                        return None;
                    }
                    body.stmt_list()?.syntax().clone()
                },
                ast::MatchExpr(it) => it.match_arm_list()?.syntax().clone(),
                ast::Impl(it) => it.assoc_item_list()?.syntax().clone(),
                ast::Trait(it) => it.assoc_item_list()?.syntax().clone(),
                ast::Module(it) => it.item_list()?.syntax().clone(),
                // Any block expression's, as rustc reads them (`may_carry_attributes` is where
                // outer ones may go), but a body's, which are its function's. The parser puts
                // them in the block's statement list.
                ast::BlockExpr(it) => {
                    if syntax.parent().is_some_and(|it| it.kind() == SyntaxKind::FN) {
                        return None;
                    }
                    it.stmt_list()?.syntax().clone()
                },
                _ => return None,
            }
        })
    }
}

/// Whether a lazy block's text may start with an inner attribute:
/// [`crate::view::block_may_start_with_inner_attr`].
fn may_start_with_inner_attr(block: &SyntaxNode) -> bool {
    match block.flat() {
        Some((tree, at)) => {
            crate::view::block_may_start_with_inner_attr(&tree.source()[tree.range(at)], false)
        }
        None => true,
    }
}

/// Returns all attributes of this node, including inner attributes that may not be directly under this node
/// but under a child.
///
/// Generic only in reading `owner`'s two nodes: the iterator is one type for every owner.
#[inline]
pub fn attrs_with_doc_including_inner(owner: &impl HasAttrs) -> AttrsWithDocIncludingInner {
    AttrsWithDocIncludingInner {
        outer: Some(owner.attrs_with_doc()),
        inner: owner.inner_attributes_node().map(|node| support::children(&node)),
    }
}

#[inline]
pub fn attrs_including_inner(owner: &impl HasAttrs) -> AttrsIncludingInner {
    AttrsIter::new(attrs_with_doc_including_inner(owner))
}

/// What [`attrs_including_inner`] gives, of any owner.
pub type AttrsIncludingInner = AttrsIter<AttrsWithDocIncludingInner>;

/// What [`attrs_with_doc_including_inner`] gives: the owner's outer attributes and doc
/// comments, then the inner ones of the node holding them.
#[derive(Clone)]
pub struct AttrsWithDocIncludingInner {
    /// `None` once its outer ones are done.
    outer: Option<AstChildren<ast::AnyAttr>>,
    inner: Option<AstChildren<ast::AnyAttr>>,
}

impl Iterator for AttrsWithDocIncludingInner {
    type Item = ast::AnyAttr;

    #[inline]
    fn next(&mut self) -> Option<ast::AnyAttr> {
        if let Some(outer) = &mut self.outer {
            match outer.find(|attr| attr.kind().is_outer()) {
                Some(attr) => return Some(attr),
                None => self.outer = None,
            }
        }
        self.inner.as_mut()?.find(|attr| attr.kind().is_inner())
    }
}

#[derive(Clone)]
pub struct AttrsIter<I> {
    inner: I,
}

impl<I: Iterator<Item = ast::AnyAttr>> AttrsIter<I> {
    #[inline]
    pub fn new(inner: I) -> Self {
        Self { inner }
    }
}

impl<I: Iterator<Item = ast::AnyAttr>> Iterator for AttrsIter<I> {
    type Item = ast::Attr;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.find_map(|attr| match attr {
            ast::AnyAttr::Attr(it) => Some(it),
            ast::AnyAttr::DocComment(_) => None,
        })
    }
}

impl<A: HasName, B: HasName> HasName for Either<A, B> {}

#[cfg(test)]
mod tests {
    use super::{
        AttrsIncludingInner, HasAttrs, attrs_including_inner, attrs_with_doc_including_inner,
    };
    use crate::{
        AstNode, Edition, SourceFile, Text,
        ast::{self, HasModuleItem},
        view::block_may_start_with_inner_attr,
    };

    #[test]
    fn a_block_may_start_with_an_inner_attribute() {
        for maybe in [
            "{ #![allow(unused)] }",
            "{#!}",
            "{ // a comment\n #![a] }",
            "{ /* a /* nested */ comment */ #![a] }",
            "{ //! inner docs\n#![a] }",
            "{ /// outer docs\n #[a] x }",
            "{ \u{2028} x }",
            "{ /* unterminated ",
            "{ // no newline",
            "not a block",
            "{",
        ] {
            assert!(block_may_start_with_inner_attr(maybe, false), "{maybe:?}");
            assert!(block_may_start_with_inner_attr(maybe, true), "{maybe:?}");
        }
    }

    #[test]
    fn an_inner_doc_comment_is_a_start_only_when_asked() {
        for docs in ["{ //! inner docs\n x }", "{ /*! inner docs */ x }"] {
            assert!(!block_may_start_with_inner_attr(docs, false), "{docs:?}");
            assert!(block_may_start_with_inner_attr(docs, true), "{docs:?}");
        }
    }

    #[test]
    fn a_block_surely_starts_with_no_inner_attribute() {
        for not in [
            "{}",
            "{ }",
            "{ let x = 1; #![a] }",
            "{ // #![a]\n x }",
            "{ /* #![a] /* #![b] */ #![c] */ x }",
            "{\r\n\t\u{0B}\u{0C} foo() }",
        ] {
            assert!(!block_may_start_with_inner_attr(not, false), "{not:?}");
            assert!(!block_may_start_with_inner_attr(not, true), "{not:?}");
        }
    }

    #[test]
    fn inner_attributes_of_a_lazy_body() {
        let text = "fn f() { let x = 1; }\nfn g() { #![allow(unused)] let y = 2; }\n";
        let outline = SourceFile::parse_outline(Text::from(text), Edition::CURRENT);
        let fns: Vec<ast::Fn> = outline
            .tree()
            .items()
            .filter_map(|it| match it {
                ast::Item::Fn(it) => Some(it),
                _ => None,
            })
            .collect();
        let parsed = |it: &ast::Fn| !it.body().unwrap().syntax().is_lazy();
        assert!(fns.iter().all(|it| it.body_is_lazy() && !parsed(it)));

        assert_eq!(attrs_including_inner(&fns[0]).count(), 0);
        assert!(fns[0].inner_attributes_node().is_none() && !parsed(&fns[0]));
        assert_eq!(attrs_including_inner(&fns[1]).count(), 1);
        assert!(parsed(&fns[1]));
    }

    #[test]
    fn the_outer_attributes_come_first_then_the_inner_ones() {
        let text = "/// outer\n#[a]\n#[b]\nmod m {\n    #![c]\n    //! inner\n    #![d]\n    #[e]\n    fn f() {}\n}\n";
        let file = SourceFile::parse(text, Edition::CURRENT);
        let module = file
            .tree()
            .items()
            .find_map(|it| match it {
                ast::Item::Module(it) => Some(it),
                _ => None,
            })
            .unwrap();
        let with_doc: Vec<(bool, bool)> = attrs_with_doc_including_inner(&module)
            .map(|it| (it.kind().is_inner(), matches!(it, ast::AnyAttr::DocComment(_))))
            .collect();
        assert_eq!(
            with_doc,
            [(false, true), (false, false), (false, false), (true, false), (true, true), (true, false)]
        );
        let names = |attrs: AttrsIncludingInner| -> Vec<String> {
            attrs.map(|it| it.meta().unwrap().syntax().text().to_string()).collect()
        };
        assert_eq!(names(attrs_including_inner(&module)), ["a", "b", "c", "d"]);
        let any = ast::AnyHasAttrs::new(module);
        assert_eq!(names(attrs_including_inner(&any)), ["a", "b", "c", "d"], "any owner, the same");
    }
}
