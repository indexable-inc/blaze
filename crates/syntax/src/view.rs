//! Typed, borrowed views of a parse's tree (frontend.md item 16): what replaces `ast` and the
//! `SyntaxNode` handles. A view is the tree it is in and an element's number there, `Copy`,
//! `Send` and `Sync`; nothing is counted, cloned or allocated to make, move or walk one. A view
//! lives as long as the borrow of the [`Parse`] (or [`FlatTree`]) it is a view of; what must
//! outlive it keeps a [`NodeId`] with its file, an `AstId`, or an [`AstPtr`] and finds the node
//! again ([`Node::at`], [`AstPtr::to_view`]).
//!
//! Callers write `use syntax::view as ast;`: the types and accessors are the handles' own
//! names, `ast::Fn<'t>` for `ast::Fn`. Every accessor takes `self` by value.

mod expr_ext;
mod generated;
mod node_ext;
pub mod prec;
mod token_ext;
mod traits;

#[cfg(test)]
mod tests;

use std::{marker::PhantomData, sync::Arc};

use either::Either;

use crate::{AstPtr, FlatTree, Parse, RustLanguage, SyntaxKind, SyntaxNodePtr, TextRange};

pub use self::{
    expr_ext::*,
    generated::{nodes::*, tokens::*},
    node_ext::*,
    token_ext::*,
    traits::*,
};
pub use crate::ast::{ArithOp, BinaryOp, CmpOp, LogicOp, Ordering, RangeOp, UnaryOp};
pub use syntax_tree::view::NodeId;

pub type Node<'t> = syntax_tree::view::Node<'t, RustLanguage>;
pub type Token<'t> = syntax_tree::view::Token<'t, RustLanguage>;
pub type Element<'t> = syntax_tree::view::Element<'t, RustLanguage>;
pub type Children<'t> = syntax_tree::view::Children<'t, RustLanguage>;
pub type ChildrenWithTokens<'t> = syntax_tree::view::ChildrenWithTokens<'t, RustLanguage>;
pub type Descendants<'t> = syntax_tree::view::Descendants<'t, RustLanguage>;
pub type DescendantsWithTokens<'t> =
    syntax_tree::view::DescendantsWithTokens<'t, RustLanguage>;
pub type DescendantsWithRanges<'t> =
    syntax_tree::view::DescendantsWithRanges<'t, RustLanguage>;
pub type Preorder<'t> = syntax_tree::view::Preorder<'t, RustLanguage>;
pub type PreorderWithTokens<'t> = syntax_tree::view::PreorderWithTokens<'t, RustLanguage>;

/// A typed view of a node: a node of the kinds it can cast from, borrowed from its tree.
pub trait AstNode<'t> {
    /// This panics if the `SyntaxKind` is not statically known.
    fn kind() -> SyntaxKind
    where
        Self: Sized,
    {
        panic!("dynamic `SyntaxKind` for `AstNode::kind()`")
    }

    fn can_cast(kind: SyntaxKind) -> bool
    where
        Self: Sized;

    fn cast(syntax: Node<'t>) -> Option<Self>
    where
        Self: Sized;

    fn syntax(&self) -> Node<'t>;
}

/// A typed view of a token.
pub trait AstToken<'t> {
    fn can_cast(kind: SyntaxKind) -> bool
    where
        Self: Sized;

    fn cast(syntax: Token<'t>) -> Option<Self>
    where
        Self: Sized;

    fn syntax(&self) -> Token<'t>;

    fn text(&self) -> &'t str {
        self.syntax().text()
    }
}

/// The child nodes of a node that are an `N`, read off the tree's arrays.
#[derive(Clone)]
pub struct AstChildren<'t, N> {
    inner: Children<'t>,
    ph: PhantomData<fn() -> N>,
}

impl<'t, N: AstNode<'t>> AstChildren<'t, N> {
    #[inline]
    fn new(parent: Node<'t>) -> Self {
        AstChildren { inner: parent.children(), ph: PhantomData }
    }
}

impl<'t, N: AstNode<'t>> Iterator for AstChildren<'t, N> {
    type Item = N;

    #[inline]
    fn next(&mut self) -> Option<N> {
        self.inner.find_map(N::cast)
    }
}

impl<'t, L: AstNode<'t>, R: AstNode<'t>> AstNode<'t> for Either<L, R> {
    fn can_cast(kind: SyntaxKind) -> bool {
        L::can_cast(kind) || R::can_cast(kind)
    }

    fn cast(syntax: Node<'t>) -> Option<Self> {
        if L::can_cast(syntax.kind()) {
            L::cast(syntax).map(Either::Left)
        } else {
            R::cast(syntax).map(Either::Right)
        }
    }

    fn syntax(&self) -> Node<'t> {
        self.as_ref().either(L::syntax, R::syntax)
    }
}

impl<'t, L: HasAttrs<'t>, R: HasAttrs<'t>> HasAttrs<'t> for Either<L, R> {}

impl<'t, L: HasName<'t>, R: HasName<'t>> HasName<'t> for Either<L, R> {}

/// Operations common to `RangeExpr` and `RangePat`.
pub trait RangeItem<'t> {
    type Bound;

    fn start(&self) -> Option<Self::Bound>;
    fn end(&self) -> Option<Self::Bound>;
    fn op_kind(&self) -> Option<RangeOp>;
    fn op_token(&self) -> Option<Token<'t>>;
}

pub(crate) mod support {
    use super::{AstChildren, AstNode, Node, SyntaxKind, Token};

    #[inline]
    pub(crate) fn child<'t, N: AstNode<'t>>(parent: Node<'t>) -> Option<N> {
        parent.children().find_map(N::cast)
    }

    #[inline]
    pub(crate) fn children<'t, N: AstNode<'t>>(parent: Node<'t>) -> AstChildren<'t, N> {
        AstChildren::new(parent)
    }

    #[inline]
    pub(crate) fn token(parent: Node<'_>, kind: SyntaxKind) -> Option<Token<'_>> {
        parent.child_token(kind)
    }
}

/// The view type of a handle type, while both exist: `<ast::Fn as Typed>::Of<'t>` is
/// `view::Fn<'t>`. Lets a `Parse<ast::SourceFile>` give its view and an `AstPtr<ast::Fn>` find
/// its node as a view.
pub trait Typed {
    type Of<'t>: AstNode<'t>;
}

impl<L: Typed, R: Typed> Typed for Either<L, R> {
    type Of<'t> = Either<L::Of<'t>, R::Of<'t>>;
}

impl Typed for crate::ast::Macro {
    type Of<'t> = Macro<'t>;
}

impl<T> Parse<T> {
    /// The root of the tree, borrowed.
    #[inline]
    pub fn root(&self) -> Node<'_> {
        Node::root(self.tree.as_ref().expect("a parse holds its tree until dropped").flat())
    }
}

impl<T: Typed> Parse<T> {
    /// The root of the tree as its typed view.
    ///
    /// # Panics
    ///
    /// When the root is not a `T` (an `ERROR` root of a fragment that did not parse).
    #[inline]
    pub fn view<'t>(&'t self) -> T::Of<'t> {
        <T::Of<'t> as AstNode<'t>>::cast(self.root()).expect("a parse's root is of its type")
    }
}

/// The tag of a parse whose root has no one kind (a macro expansion, a token tree as a fragment,
/// from [`crate::SyntaxTreeBuilder::finish_view`]): it has no view type of its own, so a
/// `Parse<Untyped>` is walked from [`Parse::root`] or cast with [`Parse::cast`]. Not a handle:
/// nothing of this type exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Untyped {}

impl<T> Parse<T> {
    /// The same parse with its kind forgotten.
    #[inline]
    pub fn into_untyped(mut self) -> Parse<Untyped> {
        Parse {
            tree: self.tree.take(),
            errors: self.errors.take(),
            outline: self.outline,
            _ty: PhantomData,
        }
    }
}

impl Parse<Untyped> {
    /// The parse as a `T`, when its root is one.
    pub fn cast<T: Typed>(mut self) -> Option<Parse<T>> {
        if !<T::Of<'_> as AstNode<'_>>::can_cast(self.root().kind()) {
            return None;
        }
        Some(Parse {
            tree: self.tree.take(),
            errors: self.errors.take(),
            outline: self.outline,
            _ty: PhantomData,
        })
    }
}

/// Typed views of the root of any flat tree, for trees not held by a [`Parse`].
#[inline]
pub fn root<'t, N: AstNode<'t>>(tree: &'t Arc<FlatTree>) -> Option<N> {
    N::cast(Node::root(tree))
}

/// A [`SyntaxNodePtr`] to `node`.
#[inline]
pub fn ptr(node: Node<'_>) -> SyntaxNodePtr {
    SyntaxNodePtr::from_parts(node.kind(), node.text_range())
}

/// rustc's span of an item or a macro call, which leaves out the attributes, doc comments and
/// comments on top of it: from the first of `children` (a node's children, in order, as their
/// kinds and ranges) that is none of those, to the end of `range`, the node's own.
pub fn written_range_of(
    range: TextRange,
    children: impl IntoIterator<Item = (SyntaxKind, TextRange)>,
) -> TextRange {
    let start = children
        .into_iter()
        .find(|&(kind, _)| {
            !matches!(kind, SyntaxKind::ATTR | SyntaxKind::DOC_COMMENT) && !kind.is_trivia()
        })
        .map_or(range.start(), |(_, it)| it.start());
    TextRange::new(start, range.end())
}

/// [`written_range_of`] of `node`.
#[inline]
pub fn written_range(node: Node<'_>) -> TextRange {
    written_range_of(
        node.text_range(),
        node.children_with_tokens().map(|it| (it.kind(), it.text_range())),
    )
}

/// rustc's span of a function's definition (`def_span`): its [`written_range`] up to the end of
/// its signature, the last element before its body that is not trivia; all of it if it has no
/// body.
pub fn signature_range(item: Fn<'_>) -> TextRange {
    let range = written_range(item.syntax());
    let end = match item.body() {
        Some(body) => {
            let mut end = body.syntax().text_range().start();
            let mut at = body.syntax().prev_sibling_or_token();
            while let Some(it) = at {
                if !it.kind().is_trivia() {
                    end = it.text_range().end();
                    break;
                }
                at = it.prev_sibling_or_token();
            }
            end
        }
        None => range.end(),
    };
    TextRange::new(range.start(), end.max(range.start()))
}

/// rustc's span of a type parameter (`GenericParam::span`, its `def_span`): its name, to the
/// end of its default if it has one. None for a parameter without a name.
pub fn type_param_span(param: TypeParam<'_>) -> Option<TextRange> {
    let name = param.name()?.syntax().text_range();
    Some(match param.default_type() {
        Some(ty) => name.cover(ty.syntax().text_range()),
        None => name,
    })
}

/// The node `ptr` points at under `root`, found from the bottom up.
///
/// # Panics
///
/// When there is none: `ptr` is of another tree.
#[inline]
pub fn to_node<'t>(ptr: SyntaxNodePtr, root: Node<'t>) -> Node<'t> {
    try_to_node(ptr, root).unwrap_or_else(|| {
        panic!("no {:?} at {:?} in {:?}", ptr.kind(), ptr.text_range(), root.text_range())
    })
}

/// The node `ptr` points at under `root`, if there is one.
#[inline]
pub fn try_to_node<'t>(ptr: SyntaxNodePtr, root: Node<'t>) -> Option<Node<'t>> {
    root.descendant_at_range(ptr.text_range(), ptr.kind())
}

impl<N: crate::AstNode + Typed> AstPtr<N> {
    /// An `AstPtr` to the node a view stands for.
    #[inline]
    #[allow(clippy::needless_pass_by_value, reason = "a view is a copied handle, taken as views are passed everywhere")]
    pub fn of<'t>(node: N::Of<'t>) -> AstPtr<N> {
        let syntax = <N::Of<'t> as AstNode<'t>>::syntax(&node);
        AstPtr::try_from_raw(ptr(syntax)).expect("a view casts to its handle type")
    }

    /// The node this points at under `root`, as a view.
    ///
    /// # Panics
    ///
    /// When there is none: the pointer is of another tree.
    #[inline]
    pub fn to_view<'t>(&self, root: Node<'t>) -> N::Of<'t> {
        let node = to_node(self.syntax_node_ptr(), root);
        <N::Of<'t> as AstNode<'t>>::cast(node).expect("a pointer's node is of its type")
    }

    #[inline]
    pub fn try_to_view<'t>(&self, root: Node<'t>) -> Option<N::Of<'t>> {
        <N::Of<'t> as AstNode<'t>>::cast(try_to_node(self.syntax_node_ptr(), root)?)
    }
}
