//! Borrowed views of a [`FlatTree`]'s elements: [`Node`] and [`Token`], `Copy`, `Send` and
//! `Sync`, each the tree it is in and its number there. Nothing is counted, cloned or
//! allocated to make one, move one or walk with one: every step is arithmetic on the tree's
//! arrays. A view lives as long as the borrow of its tree; what must outlive it keeps the
//! element's [`NodeId`] (and finds it again with [`Node::at`]) or an `AstPtr`.
//!
//! Lazy nodes are transparent, as for the cursors: going down into one enters its part
//! ([`FlatTree::part`], parsed then, once), whose root stands for the lazy node itself, and going
//! up or sideways out of a part's children comes back to the file's tree. A view never stands at
//! a part's root element: that element is the lazy node, so equal elements are equal views.

use std::{
    fmt,
    hash::{Hash, Hasher},
    iter,
    marker::PhantomData,
    sync::Arc,
};

use crate::{
    flat::FlatTree, Direction, FlatTreeBuilder, Language, NodeOrToken, SyntaxKind, TextRange,
    TextSize, TokenAtOffset, WalkEvent,
};

/// Where an element is in a file's tree: its number `at` in the file's tree when `part` is
/// [`NodeId::FILE`], else its number in the part of the file's lazy node `part`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct NodeId {
    pub part: u32,
    pub at: u32,
}

impl NodeId {
    /// The `part` of an element of the file's tree itself.
    pub const FILE: u32 = u32::MAX;
}

const _: () = assert!(std::mem::size_of::<NodeId>() == 8);

/// The tree an element is in: the file's (`part == NodeId::FILE`, `tree == file`) or the part
/// of the file's lazy node `part`.
#[derive(Clone, Copy)]
struct Place<'t> {
    file: &'t Arc<FlatTree>,
    tree: &'t FlatTree,
    part: u32,
}

impl<'t> Place<'t> {
    #[inline]
    fn file(file: &'t Arc<FlatTree>) -> Place<'t> {
        Place { file, tree: file, part: NodeId::FILE }
    }

    /// Where the children of element `at` are: in its part, under the part's root, for a lazy
    /// node. Parts have no lazy nodes of their own.
    #[inline]
    fn inner(self, at: u32) -> (Place<'t>, u32) {
        match self.tree.is_lazy(at) {
            true => (Place { file: self.file, tree: self.tree.part(at), part: at }, 0),
            false => (self, at),
        }
    }

    /// The parent of element `at`: the lazy node for a child of a part's root.
    #[inline]
    fn up(self, at: u32) -> Option<(Place<'t>, u32)> {
        let parent = self.tree.parent(at)?;
        Some(match self.part != NodeId::FILE && parent == 0 {
            true => (Place::file(self.file), self.part),
            false => (self, parent),
        })
    }

    /// Element `at` of this tree, the lazy node for a part's root.
    #[inline]
    fn resolve(self, at: u32) -> (Place<'t>, u32) {
        match self.part != NodeId::FILE && at == 0 {
            true => (Place::file(self.file), self.part),
            false => (self, at),
        }
    }

    #[inline]
    fn node<L: Language>(self, at: u32) -> Node<'t, L> {
        let (place, at) = self.resolve(at);
        Node { file: place.file, tree: place.tree, part: place.part, at, _l: PhantomData }
    }

    /// Token `at` of this tree; a part's root is a node, so no token needs resolving.
    #[inline]
    fn token<L: Language>(self, at: u32) -> Token<'t, L> {
        Token { file: self.file, tree: self.tree, part: self.part, at, _l: PhantomData }
    }

    #[inline]
    fn element<L: Language>(self, at: u32) -> Element<'t, L> {
        match self.tree.is_token(at) {
            true => NodeOrToken::Token(self.token(at)),
            false => NodeOrToken::Node(self.node(at)),
        }
    }
}

/// A node of a flat tree, borrowed: 24 bytes, `Copy`.
pub struct Node<'t, L: Language> {
    file: &'t Arc<FlatTree>,
    tree: &'t FlatTree,
    part: u32,
    at: u32,
    _l: PhantomData<L>,
}

/// A token of a flat tree, borrowed: 24 bytes, `Copy`.
pub struct Token<'t, L: Language> {
    file: &'t Arc<FlatTree>,
    tree: &'t FlatTree,
    part: u32,
    at: u32,
    _l: PhantomData<L>,
}

pub type Element<'t, L> = NodeOrToken<Node<'t, L>, Token<'t, L>>;

impl<L: Language> Clone for Node<'_, L> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<L: Language> Copy for Node<'_, L> {}

impl<L: Language> Clone for Token<'_, L> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<L: Language> Copy for Token<'_, L> {}

impl<'t, L: Language> Node<'t, L> {
    /// The root of `tree`, which is a node.
    #[inline]
    pub fn root(tree: &'t Arc<FlatTree>) -> Node<'t, L> {
        assert!(!tree.is_empty() && !tree.is_token(0), "a tree's root is a node");
        Place::file(tree).node(0)
    }

    /// The element `id` names in the file's tree `file`, parsing its part if it is in one that
    /// was not parsed yet.
    #[inline]
    pub fn at(file: &'t Arc<FlatTree>, id: NodeId) -> Element<'t, L> {
        let place = match id.part {
            NodeId::FILE => Place::file(file),
            part => Place { file, tree: file.part(part), part },
        };
        place.element(id.at)
    }

    #[inline]
    fn place(self) -> Place<'t> {
        Place { file: self.file, tree: self.tree, part: self.part }
    }

    #[inline]
    fn inner(self) -> (Place<'t>, u32) {
        self.place().inner(self.at)
    }

    /// Where this node is in its file's tree.
    #[inline]
    pub fn id(self) -> NodeId {
        NodeId { part: self.part, at: self.at }
    }

    /// The file's tree this node is in, the root's for a node in a part.
    #[inline]
    pub fn file(self) -> &'t FlatTree {
        self.file
    }

    /// [`Self::file`], shared: a count of it (`Arc::clone`) keeps the tree past this view's
    /// borrow, to be viewed again by [`Node::at`].
    #[inline]
    pub fn shared_file(self) -> &'t Arc<FlatTree> {
        self.file
    }

    /// The flat tree this node is in and its number there: a lazy node's part for a node inside
    /// it, the file's tree for the lazy node itself.
    #[inline]
    pub fn flat(self) -> (&'t FlatTree, u32) {
        (self.tree, self.at)
    }

    #[inline]
    pub fn kind(self) -> L::Kind {
        L::kind_from_raw(self.tree.kind(self.at))
    }

    #[inline]
    pub fn raw_kind(self) -> SyntaxKind {
        self.tree.kind(self.at)
    }

    #[inline]
    pub fn text_range(self) -> TextRange {
        self.tree.range(self.at)
    }

    /// The node's text: one slice of the source its tree shares.
    #[inline]
    pub fn text(self) -> &'t str {
        &self.tree.text()[self.tree.range(self.at)]
    }

    #[inline]
    pub fn index(self) -> usize {
        self.tree.index(self.at)
    }

    /// Whether this is a lazy node whose part is not parsed yet. Never parses.
    #[inline]
    pub fn is_lazy(self) -> bool {
        self.tree.is_lazy(self.at) && !self.tree.is_parsed(self.at)
    }

    #[inline]
    pub fn parent(self) -> Option<Node<'t, L>> {
        self.place().up(self.at).map(|(place, it)| place.node(it))
    }

    #[inline]
    pub fn ancestors(self) -> impl Iterator<Item = Node<'t, L>> {
        iter::successors(Some(self), |it| it.parent())
    }

    /// The root of the file's tree this node is in.
    #[inline]
    pub fn tree_top(self) -> Node<'t, L> {
        Place::file(self.file).node(0)
    }

    #[inline]
    pub fn children(self) -> Children<'t, L> {
        let (place, at) = self.inner();
        Children { place, next: at + 1, end: place.tree.end(at), _l: PhantomData }
    }

    #[inline]
    pub fn children_with_tokens(self) -> ChildrenWithTokens<'t, L> {
        let (place, at) = self.inner();
        ChildrenWithTokens { place, next: at + 1, end: place.tree.end(at), _l: PhantomData }
    }

    #[inline]
    pub fn first_child(self) -> Option<Node<'t, L>> {
        let (place, at) = self.inner();
        place.tree.first_child(at).map(|it| place.node(it))
    }

    pub fn last_child(self) -> Option<Node<'t, L>> {
        let (place, at) = self.inner();
        let tree = place.tree;
        let mut child = tree.last_child_or_token(at);
        while let Some(it) = child {
            if !tree.is_token(it) {
                return Some(place.node(it));
            }
            child = tree.prev_sibling_or_token(it);
        }
        None
    }

    #[inline]
    pub fn first_child_or_token(self) -> Option<Element<'t, L>> {
        let (place, at) = self.inner();
        place.tree.first_child_or_token(at).map(|it| place.element(it))
    }

    #[inline]
    pub fn last_child_or_token(self) -> Option<Element<'t, L>> {
        let (place, at) = self.inner();
        place.tree.last_child_or_token(at).map(|it| place.element(it))
    }

    #[inline]
    pub fn next_sibling(self) -> Option<Node<'t, L>> {
        let place = self.place();
        place.tree.next_sibling(self.at).map(|it| place.node(it))
    }

    pub fn prev_sibling(self) -> Option<Node<'t, L>> {
        let place = self.place();
        let tree = place.tree;
        let mut sibling = tree.prev_sibling_or_token(self.at);
        while let Some(it) = sibling {
            if !tree.is_token(it) {
                return Some(place.node(it));
            }
            sibling = tree.prev_sibling_or_token(it);
        }
        None
    }

    #[inline]
    pub fn next_sibling_or_token(self) -> Option<Element<'t, L>> {
        let place = self.place();
        place.tree.next_sibling_or_token(self.at).map(|it| place.element(it))
    }

    #[inline]
    pub fn prev_sibling_or_token(self) -> Option<Element<'t, L>> {
        let place = self.place();
        place.tree.prev_sibling_or_token(self.at).map(|it| place.element(it))
    }

    pub fn first_token(self) -> Option<Token<'t, L>> {
        self.first_child_or_token()?.first_token()
    }

    pub fn last_token(self) -> Option<Token<'t, L>> {
        self.last_child_or_token()?.last_token()
    }

    #[inline]
    pub fn siblings(self, direction: Direction) -> impl Iterator<Item = Node<'t, L>> {
        iter::successors(Some(self), move |node| match direction {
            Direction::Next => node.next_sibling(),
            Direction::Prev => node.prev_sibling(),
        })
    }

    #[inline]
    pub fn siblings_with_tokens(self, direction: Direction) -> impl Iterator<Item = Element<'t, L>> {
        iter::successors(Some(NodeOrToken::Node(self)), move |it: &Element<'t, L>| match direction {
            Direction::Next => it.next_sibling_or_token(),
            Direction::Prev => it.prev_sibling_or_token(),
        })
    }

    /// This node and the nodes under it, in preorder: a scan of the element numbers.
    #[inline]
    pub fn descendants(self) -> Descendants<'t, L> {
        Descendants { scan: Scan::new(self.place(), self.at), _l: PhantomData }
    }

    /// This node and the elements under it, in preorder: a scan of the element numbers.
    #[inline]
    pub fn descendants_with_tokens(self) -> DescendantsWithTokens<'t, L> {
        DescendantsWithTokens { scan: Scan::new(self.place(), self.at), _l: PhantomData }
    }

    /// [`Self::descendants_with_tokens`] with each element's range. The walk carries the
    /// running offset, so a token's range is one add and a node's end at most a kept start and
    /// seven lengths, where asking each element's `text_range` reads up to fifteen words.
    #[inline]
    pub fn descendants_with_ranges(self) -> DescendantsWithRanges<'t, L> {
        DescendantsWithRanges {
            scan: Scan::new(self.place(), self.at),
            offset: self.tree.offset(self.at),
            after_part: 0,
            _l: PhantomData,
        }
    }

    #[inline]
    pub fn preorder(self) -> Preorder<'t, L> {
        Preorder { start: self, last: None, done: false, skip_subtree: false }
    }

    #[inline]
    pub fn preorder_with_tokens(self) -> PreorderWithTokens<'t, L> {
        let start = NodeOrToken::Node(self);
        PreorderWithTokens { start, last: None, done: false, skip_subtree: false }
    }

    /// The first child node whose kind `pred` accepts.
    #[inline]
    pub fn child_where(self, mut pred: impl FnMut(L::Kind) -> bool) -> Option<Node<'t, L>> {
        let (place, at) = self.inner();
        let tree = place.tree;
        let child = tree.children(at).find(|&it| pred(L::kind_from_raw(tree.kind(it))))?;
        Some(place.node(child))
    }

    /// The nearest of this node and its ancestors whose kind `pred` accepts.
    #[inline]
    pub fn ancestor_where(self, mut pred: impl FnMut(L::Kind) -> bool) -> Option<Node<'t, L>> {
        let (mut place, mut it) = (self.place(), self.at);
        while !pred(L::kind_from_raw(place.tree.kind(it))) {
            (place, it) = place.up(it)?;
        }
        Some(place.node(it))
    }

    /// The first child token of `kind`.
    #[inline]
    pub fn child_token(self, kind: L::Kind) -> Option<Token<'t, L>> {
        let (place, at) = self.inner();
        let (tree, kind) = (place.tree, L::kind_to_raw(kind));
        let token =
            tree.children_with_tokens(at).find(|&it| tree.is_token(it) && tree.kind(it) == kind)?;
        Some(place.token(token))
    }

    /// The text of the node's first child, when that is a token.
    #[inline]
    pub fn first_child_token_text(self) -> Option<&'t str> {
        let (place, at) = self.inner();
        let tree = place.tree;
        let first = tree.first_child_or_token(at).filter(|&it| tree.is_token(it))?;
        Some(tree.token_text(first))
    }

    pub fn token_at_offset(self, offset: TextSize) -> TokenAtOffset<Token<'t, L>> {
        let range = self.text_range();
        assert!(
            range.start() <= offset && offset <= range.end(),
            "Bad offset: range {range:?} offset {offset:?}",
        );
        if range.is_empty() {
            return TokenAtOffset::None;
        }

        let mut children = self.children_with_tokens().filter(|child| {
            let child_range = child.text_range();
            !child_range.is_empty()
                && (child_range.start() <= offset && offset <= child_range.end())
        });

        let left = children.next().unwrap();
        let right = children.next();
        assert!(children.next().is_none());

        if let Some(right) = right {
            match (left.token_at_offset(offset), right.token_at_offset(offset)) {
                (TokenAtOffset::Single(left), TokenAtOffset::Single(right)) => {
                    TokenAtOffset::Between(left, right)
                }
                _ => unreachable!(),
            }
        } else {
            left.token_at_offset(offset)
        }
    }

    /// The deepest node or token under this one that holds `range`, which this node holds.
    pub fn covering_element(self, range: TextRange) -> Element<'t, L> {
        let mut res = NodeOrToken::Node(self);
        loop {
            assert!(
                res.text_range().contains_range(range),
                "Bad range: node range {:?}, range {:?}",
                res.text_range(),
                range,
            );
            res = match res {
                NodeOrToken::Token(_) => return res,
                NodeOrToken::Node(node) => match node.child_or_token_at_range(range) {
                    Some(it) => it,
                    None => return res,
                },
            };
        }
    }

    /// The child or token holding `range`, found by binary search.
    #[inline]
    pub fn child_or_token_at_range(self, range: TextRange) -> Option<Element<'t, L>> {
        let (place, at) = self.inner();
        place.tree.child_at_range(at, range).map(|it| place.element(it))
    }

    /// The first node of `kind` at `range` that stepping down from this node by
    /// [`Self::child_or_token_at_range`] finds: for a range that is not empty, from the bottom
    /// up, in steps as many as the nodes around the range no larger than it.
    pub fn descendant_at_range(self, range: TextRange, kind: L::Kind) -> Option<Node<'t, L>> {
        if range.is_empty() {
            return iter::successors(Some(self), |it| it.child_or_token_at_range(range)?.into_node())
                .find(|it| it.text_range() == range && it.kind() == kind);
        }
        let raw = L::kind_to_raw(kind);
        let (place, at) = self.inner();
        let tree = place.tree;
        if let Some(found) = tree.node_at_range(at, range, raw) {
            return Some(place.node(found));
        }
        // The arrays see a lazy node as a node without children, so what is not found above it
        // may be in its part.
        let lazy = tree
            .last_starting(at, range.start())
            .filter(|&it| tree.is_lazy(it) && tree.range(it).contains_range(range))?;
        let (part, at) = place.inner(lazy);
        part.tree.node_at_range(at, range, raw).map(|it| part.node(it))
    }

    /// An independent copy of this node's subtree: a tree of its own whose text starts at 0,
    /// its lazy nodes lazy.
    pub fn subtree(self) -> FlatTree {
        self.tree.subtree(self.at)
    }

    /// [`Self::subtree`] with the nodes `elided` under this one, ascending and disjoint, each
    /// replaced by an empty node of its kind ([`FlatTree::subtree_without`]): the cursors'
    /// `clone_subtree_without`. They must be in the tree this node's children are in (its
    /// part's, for a lazy node).
    pub fn subtree_without(self, elided: &[Node<'t, L>]) -> FlatTree {
        self.subtree_without_in(elided, None)
    }

    /// [`Self::subtree_without`] with every elided node replaced by an empty node of
    /// `placeholder`, whatever its own kind: a copy that an edit turning an elided node into one
    /// of another kind leaves the same.
    pub fn subtree_without_as(self, elided: &[Node<'t, L>], placeholder: L::Kind) -> FlatTree {
        self.subtree_without_in(elided, Some(L::kind_to_raw(placeholder)))
    }

    fn subtree_without_in(self, elided: &[Node<'t, L>], placeholder: Option<SyntaxKind>) -> FlatTree {
        if elided.is_empty() {
            return self.subtree();
        }
        let (place, at) = self.inner();
        let numbers = elided.iter().map(|node| {
            assert!(
                std::ptr::eq(node.tree, place.tree),
                "the elided {:?} at {:?} is not in the flat tree of the {:?} it is cut from",
                node.kind(),
                node.text_range(),
                self.kind(),
            );
            node.at
        });
        // alloc: the elided nodes' numbers, once per copy
        let numbers: Vec<u32> = numbers.collect();
        place.tree.subtree_without(at, &numbers, placeholder)
    }
}

impl<'t, L: Language> Token<'t, L> {
    #[inline]
    fn place(self) -> Place<'t> {
        Place { file: self.file, tree: self.tree, part: self.part }
    }

    /// Where this token is in its file's tree.
    #[inline]
    pub fn id(self) -> NodeId {
        NodeId { part: self.part, at: self.at }
    }

    #[inline]
    pub fn file(self) -> &'t FlatTree {
        self.file
    }

    /// [`Self::file`], shared, as [`Node::shared_file`].
    #[inline]
    pub fn shared_file(self) -> &'t Arc<FlatTree> {
        self.file
    }

    #[inline]
    pub fn flat(self) -> (&'t FlatTree, u32) {
        (self.tree, self.at)
    }

    #[inline]
    pub fn kind(self) -> L::Kind {
        L::kind_from_raw(self.tree.kind(self.at))
    }

    #[inline]
    pub fn raw_kind(self) -> SyntaxKind {
        self.tree.kind(self.at)
    }

    #[inline]
    pub fn text_range(self) -> TextRange {
        self.tree.range(self.at)
    }

    #[inline]
    pub fn text(self) -> &'t str {
        self.tree.token_text(self.at)
    }

    #[inline]
    pub fn index(self) -> usize {
        self.tree.index(self.at)
    }

    #[inline]
    pub fn parent(self) -> Option<Node<'t, L>> {
        self.place().up(self.at).map(|(place, it)| place.node(it))
    }

    /// The ancestors of this token, its parent first.
    #[inline]
    pub fn parent_ancestors(self) -> impl Iterator<Item = Node<'t, L>> {
        iter::successors(self.parent(), |it| it.parent())
    }

    /// [`Self::parent_ancestors`].
    #[inline]
    pub fn ancestors(self) -> impl Iterator<Item = Node<'t, L>> {
        self.parent_ancestors()
    }

    #[inline]
    pub fn tree_top(self) -> Node<'t, L> {
        Place::file(self.file).node(0)
    }

    #[inline]
    pub fn next_sibling_or_token(self) -> Option<Element<'t, L>> {
        let place = self.place();
        place.tree.next_sibling_or_token(self.at).map(|it| place.element(it))
    }

    #[inline]
    pub fn prev_sibling_or_token(self) -> Option<Element<'t, L>> {
        let place = self.place();
        place.tree.prev_sibling_or_token(self.at).map(|it| place.element(it))
    }

    #[inline]
    pub fn siblings_with_tokens(self, direction: Direction) -> impl Iterator<Item = Element<'t, L>> {
        iter::successors(Some(NodeOrToken::Token(self)), move |it: &Element<'t, L>| match direction {
            Direction::Next => it.next_sibling_or_token(),
            Direction::Prev => it.prev_sibling_or_token(),
        })
    }

    /// The next token in the tree, not necessarily a sibling.
    pub fn next_token(self) -> Option<Token<'t, L>> {
        match self.next_sibling_or_token() {
            Some(element) => element.first_token(),
            None => self
                .parent_ancestors()
                .find_map(|it| it.next_sibling_or_token())
                .and_then(|element| element.first_token()),
        }
    }

    /// The previous token in the tree, not necessarily a sibling.
    pub fn prev_token(self) -> Option<Token<'t, L>> {
        match self.prev_sibling_or_token() {
            Some(element) => element.last_token(),
            None => self
                .parent_ancestors()
                .find_map(|it| it.prev_sibling_or_token())
                .and_then(|element| element.last_token()),
        }
    }
}

impl<'t, L: Language> NodeOrToken<Node<'t, L>, Token<'t, L>> {
    #[inline]
    pub fn id(self) -> NodeId {
        match self {
            NodeOrToken::Node(it) => it.id(),
            NodeOrToken::Token(it) => it.id(),
        }
    }

    #[inline]
    pub fn text_range(self) -> TextRange {
        match self {
            NodeOrToken::Node(it) => it.text_range(),
            NodeOrToken::Token(it) => it.text_range(),
        }
    }

    #[inline]
    pub fn index(self) -> usize {
        match self {
            NodeOrToken::Node(it) => it.index(),
            NodeOrToken::Token(it) => it.index(),
        }
    }

    #[inline]
    pub fn kind(self) -> L::Kind {
        match self {
            NodeOrToken::Node(it) => it.kind(),
            NodeOrToken::Token(it) => it.kind(),
        }
    }

    #[inline]
    pub fn parent(self) -> Option<Node<'t, L>> {
        match self {
            NodeOrToken::Node(it) => it.parent(),
            NodeOrToken::Token(it) => it.parent(),
        }
    }

    /// This node and its ancestors, or this token's ancestors.
    #[inline]
    pub fn ancestors(self) -> impl Iterator<Item = Node<'t, L>> {
        let first = match self {
            NodeOrToken::Node(it) => Some(it),
            NodeOrToken::Token(it) => it.parent(),
        };
        iter::successors(first, |it| it.parent())
    }

    #[inline]
    pub fn tree_top(self) -> Node<'t, L> {
        match self {
            NodeOrToken::Node(it) => it.tree_top(),
            NodeOrToken::Token(it) => it.tree_top(),
        }
    }

    pub fn first_token(self) -> Option<Token<'t, L>> {
        match self {
            NodeOrToken::Node(it) => it.first_token(),
            NodeOrToken::Token(it) => Some(it),
        }
    }

    pub fn last_token(self) -> Option<Token<'t, L>> {
        match self {
            NodeOrToken::Node(it) => it.last_token(),
            NodeOrToken::Token(it) => Some(it),
        }
    }

    #[inline]
    pub fn next_sibling_or_token(self) -> Option<Element<'t, L>> {
        match self {
            NodeOrToken::Node(it) => it.next_sibling_or_token(),
            NodeOrToken::Token(it) => it.next_sibling_or_token(),
        }
    }

    #[inline]
    pub fn prev_sibling_or_token(self) -> Option<Element<'t, L>> {
        match self {
            NodeOrToken::Node(it) => it.prev_sibling_or_token(),
            NodeOrToken::Token(it) => it.prev_sibling_or_token(),
        }
    }

    fn token_at_offset(self, offset: TextSize) -> TokenAtOffset<Token<'t, L>> {
        assert!(self.text_range().start() <= offset && offset <= self.text_range().end());
        match self {
            NodeOrToken::Token(token) => TokenAtOffset::Single(token),
            NodeOrToken::Node(node) => node.token_at_offset(offset),
        }
    }
}

impl FlatTreeBuilder {
    /// Gives the builder a copy of `element` and its subtree, the parts of its lazy nodes in it.
    pub fn view<L: Language>(&mut self, element: Element<'_, L>) {
        let node = match element {
            NodeOrToken::Token(token) => return self.token(token.raw_kind(), token.text()),
            NodeOrToken::Node(node) => node,
        };
        for event in node.preorder_with_tokens() {
            match event {
                WalkEvent::Enter(NodeOrToken::Node(it)) => self.start_node(it.raw_kind()),
                WalkEvent::Enter(NodeOrToken::Token(it)) => self.token(it.raw_kind(), it.text()),
                WalkEvent::Leave(NodeOrToken::Node(_)) => self.finish_node(),
                WalkEvent::Leave(NodeOrToken::Token(_)) => {}
            }
        }
    }
}

// region: identity

impl<L: Language> PartialEq for Node<'_, L> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(self.file, other.file) && self.part == other.part && self.at == other.at
    }
}

impl<L: Language> Eq for Node<'_, L> {}

impl<L: Language> Hash for Node<'_, L> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        (Arc::as_ptr(self.file) as usize, self.part, self.at).hash(state);
    }
}

impl<L: Language> PartialEq for Token<'_, L> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(self.file, other.file) && self.part == other.part && self.at == other.at
    }
}

impl<L: Language> Eq for Token<'_, L> {}

impl<L: Language> Hash for Token<'_, L> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        (Arc::as_ptr(self.file) as usize, self.part, self.at).hash(state);
    }
}

impl<L: Language> fmt::Debug for Node<'_, L> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !f.alternate() {
            return write!(f, "{:?}@{:?}", self.kind(), self.text_range());
        }
        let mut level = 0;
        for event in self.preorder_with_tokens() {
            match event {
                WalkEvent::Enter(element) => {
                    for _ in 0..level {
                        write!(f, "  ")?;
                    }
                    match element {
                        NodeOrToken::Node(node) => writeln!(f, "{node:?}")?,
                        NodeOrToken::Token(token) => writeln!(f, "{token:?}")?,
                    }
                    level += 1;
                }
                WalkEvent::Leave(_) => level -= 1,
            }
        }
        Ok(())
    }
}

impl<L: Language> fmt::Display for Node<'_, L> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.text())
    }
}

impl<L: Language> fmt::Debug for Token<'_, L> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}@{:?}", self.kind(), self.text_range())?;
        let text = self.text();
        if text.len() < 25 {
            return write!(f, " {text:?}");
        }
        let cut = (21..25).find(|&it| text.is_char_boundary(it)).expect("a char is 4 bytes at most");
        // alloc: debug output only, quoted as the cursors quote it.
        let text = format!("{} ...", &text[..cut]);
        write!(f, " {text:?}")
    }
}

impl<L: Language> fmt::Display for Token<'_, L> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.text())
    }
}

impl<'t, L: Language> From<Node<'t, L>> for Element<'t, L> {
    #[inline]
    fn from(node: Node<'t, L>) -> Element<'t, L> {
        NodeOrToken::Node(node)
    }
}

impl<'t, L: Language> From<Token<'t, L>> for Element<'t, L> {
    #[inline]
    fn from(token: Token<'t, L>) -> Element<'t, L> {
        NodeOrToken::Token(token)
    }
}

// endregion

// region: iterators

/// The child nodes of a node, stepping over each child's subtree by its size.
#[derive(Clone)]
pub struct Children<'t, L: Language> {
    place: Place<'t>,
    next: u32,
    end: u32,
    _l: PhantomData<L>,
}

impl<'t, L: Language> Iterator for Children<'t, L> {
    type Item = Node<'t, L>;

    #[inline]
    fn next(&mut self) -> Option<Node<'t, L>> {
        let tree = self.place.tree;
        while self.next < self.end {
            let it = self.next;
            self.next = tree.end(it);
            if !tree.is_token(it) {
                return Some(self.place.node(it));
            }
        }
        None
    }
}

/// The children and tokens of a node, stepping over each child's subtree by its size.
#[derive(Clone)]
pub struct ChildrenWithTokens<'t, L: Language> {
    place: Place<'t>,
    next: u32,
    end: u32,
    _l: PhantomData<L>,
}

impl<'t, L: Language> Iterator for ChildrenWithTokens<'t, L> {
    type Item = Element<'t, L>;

    #[inline]
    fn next(&mut self) -> Option<Element<'t, L>> {
        let it = self.next;
        if it >= self.end {
            return None;
        }
        self.next = self.place.tree.end(it);
        Some(self.place.element(it))
    }
}

/// A scan of the element numbers of a subtree in preorder, into the part of each lazy node it
/// passes and back out (parts have no lazy nodes, so one level).
#[derive(Clone)]
struct Scan<'t> {
    place: Place<'t>,
    next: u32,
    end: u32,
    /// Where the scan goes on in the file's tree once the part it is in ends.
    resume: Option<(u32, u32)>,
}

impl<'t> Scan<'t> {
    #[inline]
    fn new(place: Place<'t>, at: u32) -> Scan<'t> {
        Scan { place, next: at, end: place.tree.end(at), resume: None }
    }

    #[inline]
    fn step(&mut self, tokens: bool) -> Option<(Place<'t>, u32)> {
        loop {
            while self.next < self.end {
                let (place, it) = (self.place, self.next);
                self.next += 1;
                if place.tree.is_lazy(it) {
                    let (part, _) = place.inner(it);
                    self.resume = Some((self.next, self.end));
                    (self.place, self.next, self.end) = (part, 1, part.tree.len());
                    return Some((place, it));
                }
                if tokens || !place.tree.is_token(it) {
                    return Some((place, it));
                }
            }
            let (next, end) = self.resume.take()?;
            (self.place, self.next, self.end) = (Place::file(self.place.file), next, end);
        }
    }
}

#[derive(Clone)]
pub struct Descendants<'t, L: Language> {
    scan: Scan<'t>,
    _l: PhantomData<L>,
}

impl<'t, L: Language> Iterator for Descendants<'t, L> {
    type Item = Node<'t, L>;

    #[inline]
    fn next(&mut self) -> Option<Node<'t, L>> {
        self.scan.step(false).map(|(place, it)| place.node(it))
    }
}

#[derive(Clone)]
pub struct DescendantsWithTokens<'t, L: Language> {
    scan: Scan<'t>,
    _l: PhantomData<L>,
}

impl<'t, L: Language> Iterator for DescendantsWithTokens<'t, L> {
    type Item = Element<'t, L>;

    #[inline]
    fn next(&mut self) -> Option<Element<'t, L>> {
        self.scan.step(true).map(|(place, it)| place.element(it))
    }
}

#[derive(Clone)]
pub struct DescendantsWithRanges<'t, L: Language> {
    scan: Scan<'t>,
    /// Where the next element starts.
    offset: u32,
    /// Where the lazy node whose part the scan is in ends.
    after_part: u32,
    _l: PhantomData<L>,
}

impl<'t, L: Language> Iterator for DescendantsWithRanges<'t, L> {
    type Item = (Element<'t, L>, TextRange);

    #[inline]
    fn next(&mut self) -> Option<(Element<'t, L>, TextRange)> {
        let in_part = self.scan.place.part != NodeId::FILE;
        let (place, it) = self.scan.step(true)?;
        if in_part && place.part == NodeId::FILE {
            self.offset = self.after_part;
        }
        let tree = place.tree;
        let start = self.offset;
        let end = if tree.is_token(it) {
            self.offset += tree.text_len(it);
            self.offset
        } else if tree.is_lazy(it) {
            // A part's elements index the file's source, so its children start where it does.
            self.after_part = start + tree.text_len(it);
            self.after_part
        } else {
            tree.node_end(it, start)
        };
        Some((place.element(it), TextRange::new(TextSize::from(start), TextSize::from(end))))
    }
}

/// A walk of a node's subtree in preorder. Each step is taken when asked for, after a
/// [`Preorder::skip_subtree`] of the one before: entering a lazy node never moves into it, so
/// skipping it leaves its part unparsed.
#[derive(Clone)]
pub struct Preorder<'t, L: Language> {
    start: Node<'t, L>,
    /// The event returned last, `None` before the first and after the last.
    last: Option<WalkEvent<Node<'t, L>>>,
    done: bool,
    skip_subtree: bool,
}

impl<L: Language> Preorder<'_, L> {
    /// Leaves the node entered last without walking its subtree; after a leave, leaves the
    /// parent without walking the siblings after.
    pub fn skip_subtree(&mut self) {
        self.skip_subtree = true;
    }
}

impl<'t, L: Language> Iterator for Preorder<'t, L> {
    type Item = WalkEvent<Node<'t, L>>;

    fn next(&mut self) -> Option<WalkEvent<Node<'t, L>>> {
        let skip = std::mem::take(&mut self.skip_subtree);
        let event = match self.last {
            None if self.done => None,
            None => Some(WalkEvent::Enter(self.start)),
            Some(WalkEvent::Enter(node)) => {
                Some(match if skip { None } else { node.first_child() } {
                    Some(child) => WalkEvent::Enter(child),
                    None => WalkEvent::Leave(node),
                })
            }
            Some(WalkEvent::Leave(node)) if node == self.start => None,
            Some(WalkEvent::Leave(node)) => match if skip { None } else { node.next_sibling() } {
                Some(sibling) => Some(WalkEvent::Enter(sibling)),
                None => node.parent().map(WalkEvent::Leave),
            },
        };
        (self.last, self.done) = (event, event.is_none());
        event
    }
}

/// [`Preorder`] with tokens, each step taken when asked for as there.
#[derive(Clone)]
pub struct PreorderWithTokens<'t, L: Language> {
    start: Element<'t, L>,
    /// The event returned last, `None` before the first and after the last.
    last: Option<WalkEvent<Element<'t, L>>>,
    done: bool,
    skip_subtree: bool,
}

impl<L: Language> PreorderWithTokens<'_, L> {
    /// As [`Preorder::skip_subtree`].
    pub fn skip_subtree(&mut self) {
        self.skip_subtree = true;
    }
}

impl<'t, L: Language> Iterator for PreorderWithTokens<'t, L> {
    type Item = WalkEvent<Element<'t, L>>;

    fn next(&mut self) -> Option<WalkEvent<Element<'t, L>>> {
        let skip = std::mem::take(&mut self.skip_subtree);
        let event = match self.last {
            None if self.done => None,
            None => Some(WalkEvent::Enter(self.start)),
            Some(WalkEvent::Enter(NodeOrToken::Node(node))) => {
                Some(match if skip { None } else { node.first_child_or_token() } {
                    Some(child) => WalkEvent::Enter(child),
                    None => WalkEvent::Leave(NodeOrToken::Node(node)),
                })
            }
            Some(WalkEvent::Enter(token)) => Some(WalkEvent::Leave(token)),
            Some(WalkEvent::Leave(el)) if el == self.start => None,
            Some(WalkEvent::Leave(el)) => {
                match if skip { None } else { el.next_sibling_or_token() } {
                    Some(sibling) => Some(WalkEvent::Enter(sibling)),
                    None => el.parent().map(|it| WalkEvent::Leave(NodeOrToken::Node(it))),
                }
            }
        };
        (self.last, self.done) = (event, event.is_none());
        event
    }
}

// endregion

#[cfg(test)]
mod tests;
