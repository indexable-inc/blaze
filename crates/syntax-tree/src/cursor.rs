//! The cursors: [`SyntaxNode`] and [`SyntaxToken`], an element of a [`FlatTree`] each.
//!
//! A cursor is the tree's root and an element's number, and moving is arithmetic on the
//! tree's arrays, with nothing allocated. Trees are immutable: an edit makes a new tree
//! ([`SyntaxNode::splice_children`], [`FlatTree::replace_subtree`]).
//!
//! A flat tree's lazy nodes are transparent: moving down into one moves into its part
//! (parsed then, once), whose root stands for the lazy node itself, and moving up or
//! sideways out of a part's children moves back into the file's tree.

// Implementation notes:
//
// A `FlatRoot` is counted by every cursor into its tree, and owns the tree. Cursors are not
// `Send`: the count is a `Cell`. Two roots of one tree have equal cursors: a cursor's identity
// is its tree's address and its number.
//
// A cursor into a lazy node's part points at a `FlatRoot` of the part, which the file's
// `FlatRoot` makes the first time a cursor enters the part and owns: such a root has no
// count of its own, and a cursor into it counts the file's root, so it lives as long as any
// cursor into the file. A cursor never points at a part's root element: that element is the
// lazy node, so a cursor at it points at the lazy node in the file's tree (`flat_ref`), and
// equal elements are equal cursors. Parts have no lazy nodes of their own (`FlatTree::part`
// checks), so a part's root is always a file root's.
//
// The unsafe here: every `NonNull<FlatRoot>` this module holds or passes is a root that a
// cursor the holder owns or borrows keeps alive (its own root's count, or its file root's for a
// part's root), so `flat` reads it as a shared reference; roots are boxes freed only in
// `Ref::drop` (a file's, at its last count) and `FlatRoot::drop` (its parts'). What the safe
// version costs: `Rc` roots with the file's parts as `Weak`s (a part must not own its file
// and be owned by it) free a part's root when its last cursor goes and allocate it again on
// the next entry, an allocation per entry where this has one per part; a cursor naming its
// part by its lazy node's index instead of a pointer finds its tree by a lookup through the
// file's tree on every step, several dependent loads where this has one.

use std::{
    cell::{Cell, RefCell},
    fmt,
    hash::{Hash, Hasher},
    iter,
    ops::Range,
    ptr,
    sync::Arc,
};

use blaze_hash::HashMap;

use crate::{
    flat::FlatTree, Direction, FlatTreeBuilder, NodeOrToken, SyntaxKind, SyntaxText, TextRange,
    TextSize, TokenAtOffset, WalkEvent,
};

struct FlatRoot {
    /// The cursors into this tree and its parts; unused in a part's root.
    rc: Cell<u32>,
    tree: Arc<FlatTree>,
    /// For a part's root: the file's root, which owns it, and the lazy node it is the part of.
    file: Option<(ptr::NonNull<FlatRoot>, u32)>,
    /// For a file's root: the roots of the parts cursors entered, by their lazy node.
    parts: RefCell<HashMap<u32, ptr::NonNull<FlatRoot>>>,
}

impl Drop for FlatRoot {
    fn drop(&mut self) {
        for (_, part) in self.parts.get_mut().drain() {
            // SAFETY: a part's root is made by `part_root` from a box and freed only here, once
            // (`drain`), when no cursor counts its file's root, so none points into the part.
            // Freeing it earlier would leave those cursors reading freed memory.
            drop(unsafe { Box::from_raw(part.as_ptr()) })
        }
    }
}

/// What a cursor points at: element `at` of the tree of `root`.
struct Ref {
    root: ptr::NonNull<FlatRoot>,
    at: u32,
}

impl Clone for Ref {
    #[inline]
    fn clone(&self) -> Self {
        flat(&owner(self.root)).inc_rc();
        Ref { root: self.root, at: self.at }
    }
}

impl Drop for Ref {
    #[inline]
    fn drop(&mut self) {
        let owner = owner(self.root);
        if flat(&owner).dec_rc() {
            // SAFETY: a file's root is made by `new_flat_root` from a box, and this was the
            // last cursor counting it (every cursor into its parts counts it too), so nothing
            // reads it after. A count off by one would free it under a live cursor.
            drop(unsafe { Box::from_raw(owner.as_ptr()) })
        }
    }
}

#[inline]
fn flat(root: &ptr::NonNull<FlatRoot>) -> &FlatRoot {
    // SAFETY: a root lives while a cursor counts it (its own, or its file's for a part's), and
    // every root this module passes is kept so by a cursor its holder has (the module's
    // notes); the reference is borrowed from that pointer. Roots are only read through shared
    // references (counts and parts are cells). A root freed early would be read after free.
    unsafe { root.as_ref() }
}

/// The root that owns `root` and counts the cursors into it: the file's for a part's.
#[inline]
fn owner(root: ptr::NonNull<FlatRoot>) -> ptr::NonNull<FlatRoot> {
    match flat(&root).file {
        Some((file, _)) => file,
        None => root,
    }
}

impl FlatRoot {
    /// The root of the part of lazy node `at` of this file's tree, made the first time.
    fn part_root(&self, this: ptr::NonNull<FlatRoot>, at: u32) -> ptr::NonNull<FlatRoot> {
        debug_assert!(self.file.is_none(), "a part has no lazy nodes");
        if let Some(&part) = self.parts.borrow().get(&at) {
            return part;
        }
        // clone: once per part, the first time a cursor enters it
        let tree = Arc::clone(self.tree.part(at));
        let part = FlatRoot {
            rc: Cell::new(0),
            tree,
            file: Some((this, at)),
            parts: RefCell::default(),
        };
        // alloc: once per part, the first time a cursor enters it
        let part = ptr::NonNull::from(Box::leak(Box::new(part)));
        self.parts.borrow_mut().insert(at, part);
        part
    }

    #[inline]
    fn inc_rc(&self) {
        let rc = match self.rc.get().checked_add(1) {
            Some(it) => it,
            None => std::process::abort(),
        };
        self.rc.set(rc)
    }

    #[inline]
    fn dec_rc(&self) -> bool {
        let rc = self.rc.get() - 1;
        self.rc.set(rc);
        rc == 0
    }
}

/// A cursor at element `at` of the flat tree `root`: at the lazy node for a part's root.
#[inline]
fn flat_ref(root: ptr::NonNull<FlatRoot>, at: u32) -> Ref {
    let (root, at) = match flat(&root).file {
        Some(lazy) if at == 0 => lazy,
        _ => (root, at),
    };
    flat(&owner(root)).inc_rc();
    Ref { root, at }
}

#[inline]
fn flat_element(root: ptr::NonNull<FlatRoot>, at: u32) -> SyntaxElement {
    let token = flat(&root).tree.is_token(at);
    let element = flat_ref(root, at);
    match token {
        true => NodeOrToken::Token(SyntaxToken(element)),
        false => NodeOrToken::Node(SyntaxNode(element)),
    }
}

/// Where the children of element `at` of `root` are: a lazy node's in its part, under the
/// part's root.
#[inline]
fn inner(root: ptr::NonNull<FlatRoot>, at: u32) -> (ptr::NonNull<FlatRoot>, u32) {
    match flat(&root).tree.is_lazy(at) {
        true => (flat(&root).part_root(root, at), 0),
        false => (root, at),
    }
}

/// The parent of element `at` of `root`: the lazy node for a child of a part's root.
#[inline]
fn up(root: ptr::NonNull<FlatRoot>, at: u32) -> Option<(ptr::NonNull<FlatRoot>, u32)> {
    let parent = flat(&root).tree.parent(at)?;
    Some(match flat(&root).file {
        Some(lazy) if parent == 0 => lazy,
        _ => (root, parent),
    })
}

impl Ref {
    /// [`inner`] of this cursor, the tree borrowed from the cursor: where its children are.
    #[inline]
    fn inner_tree(&self) -> (&FlatTree, u32) {
        let (root, at) = inner(self.root, self.at);
        // SAFETY: `root` is this cursor's root or a part's root its file's root owns, and this
        // cursor counts that file's root, so `root` lives while `self` is borrowed; it is only
        // read through shared references. A part's root freed under a live cursor into its
        // file would be read after free.
        (&*unsafe { root.as_ref() }.tree, at)
    }
}

/// What stepping down by `child_or_token_at_range` from element `at` of `root` finds first
/// of `kind` and `range`, a range that is not empty. The arrays' search sees a lazy node as
/// a node without children, so when it finds nothing and the range is inside a lazy node,
/// the search goes on in its part: what is above the lazy node was searched already.
fn flat_node_at_range(
    root: ptr::NonNull<FlatRoot>,
    at: u32,
    range: TextRange,
    kind: SyntaxKind,
) -> Option<SyntaxNode> {
    let (root, at) = inner(root, at);
    let tree = &flat(&root).tree;
    if let Some(found) = tree.node_at_range(at, range, kind) {
        return Some(SyntaxNode(flat_ref(root, found)));
    }
    let lazy = tree
        .last_starting(at, range.start())
        .filter(|&it| tree.is_lazy(it) && tree.range(it).contains_range(range))?;
    let (part, at) = inner(root, lazy);
    flat(&part).tree.node_at_range(at, range, kind).map(|it| SyntaxNode(flat_ref(part, it)))
}

/// Gives `builder` `element` and its subtree, the parts of its lazy nodes in it.
pub(crate) fn copy_element(builder: &mut FlatTreeBuilder, element: &SyntaxElement) {
    match element {
        NodeOrToken::Token(token) => builder.token(token.kind(), token.text()),
        NodeOrToken::Node(node) => copy_node(builder, node),
    }
}

/// Gives `builder` `node` and its subtree, as [`copy_element`].
pub(crate) fn copy_node(builder: &mut FlatTreeBuilder, node: &SyntaxNode) {
    for event in node.preorder_with_tokens() {
        match event {
            WalkEvent::Enter(NodeOrToken::Node(it)) => builder.start_node(it.kind()),
            WalkEvent::Enter(NodeOrToken::Token(it)) => builder.token(it.kind(), it.text()),
            WalkEvent::Leave(NodeOrToken::Node(_)) => builder.finish_node(),
            WalkEvent::Leave(NodeOrToken::Token(_)) => {}
        }
    }
}

pub type SyntaxElement = NodeOrToken<SyntaxNode, SyntaxToken>;

#[derive(Clone)]
pub struct SyntaxNode(Ref);

#[derive(Clone)]
pub struct SyntaxToken(Ref);

impl Ref {
    #[inline]
    fn tree(&self) -> &FlatTree {
        &flat(&self.root).tree
    }

    #[inline]
    fn kind(&self) -> SyntaxKind {
        self.tree().kind(self.at)
    }

    #[inline]
    fn text_range(&self) -> TextRange {
        self.tree().range(self.at)
    }

    #[inline]
    fn index(&self) -> usize {
        self.tree().index(self.at)
    }

    #[inline]
    fn parent(&self) -> Option<SyntaxNode> {
        up(self.root, self.at).map(|(root, it)| SyntaxNode(flat_ref(root, it)))
    }

    #[inline]
    fn next_sibling_or_token(&self) -> Option<SyntaxElement> {
        self.tree().next_sibling_or_token(self.at).map(|it| flat_element(self.root, it))
    }

    #[inline]
    fn prev_sibling_or_token(&self) -> Option<SyntaxElement> {
        self.tree().prev_sibling_or_token(self.at).map(|it| flat_element(self.root, it))
    }

    #[inline]
    fn key(&self) -> (usize, u32) {
        (Arc::as_ptr(&flat(&self.root).tree) as usize, self.at)
    }
}

impl SyntaxNode {
    /// The root of a flat tree.
    pub fn new_flat_root(tree: Arc<FlatTree>) -> SyntaxNode {
        assert!(!tree.is_empty() && !tree.is_token(0), "a tree's root is a node");
        let root = FlatRoot { rc: Cell::new(1), tree, file: None, parts: RefCell::default() };
        // alloc: once per tree, its root's count and parts
        let root = ptr::NonNull::from(Box::leak(Box::new(root)));
        SyntaxNode(Ref { root, at: 0 })
    }

    /// Whether this is a lazy node whose part is not parsed yet. Never parses.
    #[inline]
    pub fn is_lazy(&self) -> bool {
        let tree = self.0.tree();
        tree.is_lazy(self.0.at) && !tree.is_parsed(self.0.at)
    }

    /// The flat tree this node is in: a lazy node's part for a node inside it. Always `Some`.
    pub fn flat_tree(&self) -> Option<&Arc<FlatTree>> {
        Some(&flat(&self.0.root).tree)
    }

    /// The flat tree this node is in and the node's number in it: a lazy node's part for a
    /// node inside it, the file's tree for the lazy node itself. Always `Some`.
    #[inline]
    pub fn flat(&self) -> Option<(&Arc<FlatTree>, u32)> {
        Some((&flat(&self.0.root).tree, self.0.at))
    }

    /// Element `at` of the flat tree this node is in.
    #[inline]
    pub fn flat_element(&self, at: u32) -> SyntaxElement {
        flat_element(self.0.root, at)
    }

    /// This node, for a walk that reads the arrays: every node is in a flat tree.
    #[inline]
    pub fn to_flat(&self) -> SyntaxNode {
        // clone: the owned cursor the signature returns, a count bump
        self.clone()
    }

    /// The first child node whose kind `pred` accepts.
    #[inline]
    pub fn child_where(&self, mut pred: impl FnMut(SyntaxKind) -> bool) -> Option<SyntaxNode> {
        let (root, at) = inner(self.0.root, self.0.at);
        let tree = &flat(&root).tree;
        let child = tree.children(at).find(|&it| pred(tree.kind(it)))?;
        Some(SyntaxNode(flat_ref(root, child)))
    }

    /// The nearest of this node and its ancestors whose kind `pred` accepts.
    #[inline]
    pub fn ancestor_where(&self, mut pred: impl FnMut(SyntaxKind) -> bool) -> Option<SyntaxNode> {
        let (mut root, mut it) = (self.0.root, self.0.at);
        while !pred(flat(&root).tree.kind(it)) {
            (root, it) = up(root, it)?;
        }
        Some(SyntaxNode(flat_ref(root, it)))
    }

    /// The first child token of `kind`.
    #[inline]
    pub fn child_token(&self, kind: SyntaxKind) -> Option<SyntaxToken> {
        let (root, at) = inner(self.0.root, self.0.at);
        let tree = &flat(&root).tree;
        let token =
            tree.children_with_tokens(at).find(|&it| tree.is_token(it) && tree.kind(it) == kind)?;
        Some(SyntaxToken(flat_ref(root, token)))
    }

    /// The child nodes, each step taking the next whose kind a predicate accepts.
    #[inline]
    pub fn children_where(&self) -> SyntaxNodeChildrenWhere {
        let (root, at) = inner(self.0.root, self.0.at);
        let end = flat(&root).tree.end(at);
        // clone: keeps `root` alive while the iterator lives, a count bump
        SyntaxNodeChildrenWhere { _parent: self.clone(), root, next: at + 1, end }
    }

    /// An independent copy of the subtree rooted at this node: a tree of its own whose text
    /// starts at 0, its lazy nodes lazy.
    pub fn clone_subtree(&self) -> SyntaxNode {
        let (root, at) = (self.0.root, self.0.at);
        let tree = &flat(&root).tree;
        let whole = flat(&root).file.is_none() && tree.range(0).start() == TextSize::from(0);
        match at == 0 && whole {
            true => SyntaxNode(flat_ref(root, 0)),
            // alloc: the copy's tree, shared by its cursors as every tree is
            false => SyntaxNode::new_flat_root(Arc::new(tree.subtree(at))),
        }
    }

    /// [`SyntaxNode::clone_subtree`] with the nodes `elided` under this one, in order and
    /// disjoint, each replaced by an empty node of its kind (`FlatTree::subtree_without`):
    /// what is left of the copy's text and elements, made without copying the elided ones.
    /// They must be in this node's flat tree (its part's, for a lazy node).
    pub fn clone_subtree_without<'a>(
        &self,
        elided: impl ExactSizeIterator<Item = &'a SyntaxNode>,
    ) -> SyntaxNode {
        if elided.len() == 0 {
            return self.clone_subtree();
        }
        let (root, at) = inner(self.0.root, self.0.at);
        let tree = &flat(&root).tree;
        let numbers = elided.map(|node| {
            let (of, it) = (&flat(&node.0.root).tree, node.0.at);
            assert!(
                Arc::ptr_eq(of, tree),
                "the elided {:?} at {:?} is not in the flat tree of the {:?} it is cut from",
                node.kind(),
                node.text_range(),
                self.kind(),
            );
            it
        });
        // alloc: the elided nodes' numbers, once per copy
        let numbers: Vec<u32> = numbers.collect();
        // alloc: the copy's tree, shared by its cursors as every tree is
        SyntaxNode::new_flat_root(Arc::new(tree.subtree_without(at, &numbers, None)))
    }

    /// The root of a new tree, the one this node is in but with this node's children (and
    /// tokens) at `range` replaced by copies of `with`. Only the text and structure carry
    /// over: the new tree's text is its own, its lazy nodes parsed into it, so its ranges
    /// start at 0 whatever this tree's did.
    pub fn splice_children(&self, range: Range<usize>, with: &[SyntaxElement]) -> SyntaxNode {
        self.splice_children_with(range, |builder| {
            with.iter().for_each(|it| copy_element(builder, it))
        })
    }

    /// [`SyntaxNode::splice_children`] with `with` giving the builder the copies, once.
    pub(crate) fn splice_children_with(
        &self,
        range: Range<usize>,
        mut with: impl FnMut(&mut FlatTreeBuilder),
    ) -> SyntaxNode {
        let top = self.tree_top();
        let mut builder = FlatTreeBuilder::with_capacity(usize::from(top.text_range().len()));
        let mut walk = top.preorder_with_tokens();
        while let Some(event) = walk.next() {
            match event {
                WalkEvent::Enter(NodeOrToken::Node(node)) if node == *self => {
                    builder.start_node(node.kind());
                    let len = node.children_with_tokens().count();
                    assert!(
                        range.start <= range.end && range.end <= len,
                        "children {range:?} of a {:?} of {} children",
                        node.kind(),
                        len,
                    );
                    for (i, it) in node.children_with_tokens().enumerate() {
                        if i == range.start {
                            with(&mut builder);
                        }
                        if !range.contains(&i) {
                            copy_element(&mut builder, &it);
                        }
                    }
                    if range.start == len {
                        with(&mut builder);
                    }
                    // The walk's next event is now leaving `node`, which finishes it.
                    walk.skip_subtree();
                }
                WalkEvent::Enter(NodeOrToken::Node(it)) => builder.start_node(it.kind()),
                WalkEvent::Enter(NodeOrToken::Token(it)) => builder.token(it.kind(), it.text()),
                WalkEvent::Leave(NodeOrToken::Node(_)) => builder.finish_node(),
                WalkEvent::Leave(NodeOrToken::Token(_)) => {}
            }
        }
        // alloc: the spliced tree, shared by its cursors as every tree is
        SyntaxNode::new_flat_root(Arc::new(builder.finish()))
    }

    #[inline]
    pub fn kind(&self) -> SyntaxKind {
        self.0.kind()
    }

    #[inline]
    pub fn text_range(&self) -> TextRange {
        self.0.text_range()
    }

    #[inline]
    pub fn index(&self) -> usize {
        self.0.index()
    }

    #[inline]
    pub fn text(&self) -> SyntaxText {
        // clone: the text holds its node, a count bump
        SyntaxText::new(self.clone())
    }

    /// The text of the node's first child, when that is a token.
    #[inline]
    pub fn first_child_token_text(&self) -> Option<&str> {
        let (tree, at) = self.0.inner_tree();
        let first = tree.first_child_or_token(at).filter(|&it| tree.is_token(it))?;
        Some(tree.token_text(first))
    }

    #[inline]
    pub fn parent(&self) -> Option<SyntaxNode> {
        self.0.parent()
    }

    #[inline]
    pub fn ancestors(&self) -> impl Iterator<Item = SyntaxNode> {
        // clone: the walk's owned first cursor, a count bump
        iter::successors(Some(self.clone()), SyntaxNode::parent)
    }

    #[inline]
    pub fn tree_top(&self) -> SyntaxNode {
        SyntaxNode(flat_ref(owner(self.0.root), 0))
    }

    #[inline]
    pub fn children(&self) -> SyntaxNodeChildren {
        SyntaxNodeChildren::new(self)
    }

    #[inline]
    pub fn children_with_tokens(&self) -> SyntaxElementChildren {
        SyntaxElementChildren::new(self)
    }

    pub fn first_child(&self) -> Option<SyntaxNode> {
        let (root, at) = inner(self.0.root, self.0.at);
        flat(&root).tree.first_child(at).map(|it| SyntaxNode(flat_ref(root, it)))
    }

    pub fn last_child(&self) -> Option<SyntaxNode> {
        let (root, at) = inner(self.0.root, self.0.at);
        let tree = &flat(&root).tree;
        let mut child = tree.last_child_or_token(at);
        while let Some(it) = child {
            if !tree.is_token(it) {
                return Some(SyntaxNode(flat_ref(root, it)));
            }
            child = tree.prev_sibling_or_token(it);
        }
        None
    }

    pub fn first_child_or_token(&self) -> Option<SyntaxElement> {
        let (root, at) = inner(self.0.root, self.0.at);
        flat(&root).tree.first_child_or_token(at).map(|it| flat_element(root, it))
    }

    pub fn last_child_or_token(&self) -> Option<SyntaxElement> {
        let (root, at) = inner(self.0.root, self.0.at);
        flat(&root).tree.last_child_or_token(at).map(|it| flat_element(root, it))
    }

    pub fn next_sibling(&self) -> Option<SyntaxNode> {
        let root = self.0.root;
        flat(&root).tree.next_sibling(self.0.at).map(|it| SyntaxNode(flat_ref(root, it)))
    }

    pub fn prev_sibling(&self) -> Option<SyntaxNode> {
        let root = self.0.root;
        let tree = &flat(&root).tree;
        let mut sibling = tree.prev_sibling_or_token(self.0.at);
        while let Some(it) = sibling {
            if !tree.is_token(it) {
                return Some(SyntaxNode(flat_ref(root, it)));
            }
            sibling = tree.prev_sibling_or_token(it);
        }
        None
    }

    pub fn next_sibling_or_token(&self) -> Option<SyntaxElement> {
        self.0.next_sibling_or_token()
    }

    pub fn prev_sibling_or_token(&self) -> Option<SyntaxElement> {
        self.0.prev_sibling_or_token()
    }

    pub fn first_token(&self) -> Option<SyntaxToken> {
        self.first_child_or_token()?.first_token()
    }

    pub fn last_token(&self) -> Option<SyntaxToken> {
        self.last_child_or_token()?.last_token()
    }

    #[inline]
    pub fn siblings(&self, direction: Direction) -> impl Iterator<Item = SyntaxNode> {
        // clone: the walk's owned first cursor, a count bump
        iter::successors(Some(self.clone()), move |node| match direction {
            Direction::Next => node.next_sibling(),
            Direction::Prev => node.prev_sibling(),
        })
    }

    #[inline]
    pub fn siblings_with_tokens(
        &self,
        direction: Direction,
    ) -> impl Iterator<Item = SyntaxElement> {
        // clone: the walk's owned first cursor, a count bump
        let me: SyntaxElement = self.clone().into();
        iter::successors(Some(me), move |el| match direction {
            Direction::Next => el.next_sibling_or_token(),
            Direction::Prev => el.prev_sibling_or_token(),
        })
    }

    #[inline]
    pub fn descendants(&self) -> impl Iterator<Item = SyntaxNode> {
        self.preorder().filter_map(|event| match event {
            WalkEvent::Enter(node) => Some(node),
            WalkEvent::Leave(_) => None,
        })
    }

    #[inline]
    pub fn descendants_with_tokens(&self) -> impl Iterator<Item = SyntaxElement> {
        self.preorder_with_tokens().filter_map(|event| match event {
            WalkEvent::Enter(it) => Some(it),
            WalkEvent::Leave(_) => None,
        })
    }

    #[inline]
    pub fn preorder(&self) -> Preorder {
        // clone: the walk's owned start, a count bump
        Preorder::new(self.clone())
    }

    #[inline]
    pub fn preorder_with_tokens(&self) -> PreorderWithTokens {
        // clone: the walk's owned start, a count bump
        PreorderWithTokens::new(self.clone())
    }

    pub fn token_at_offset(&self, offset: TextSize) -> TokenAtOffset<SyntaxToken> {
        let range = self.text_range();
        assert!(
            range.start() <= offset && offset <= range.end(),
            "Bad offset: range {:?} offset {:?}",
            range,
            offset
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

    pub fn covering_element(&self, range: TextRange) -> SyntaxElement {
        // clone: the owned element returned when no child covers the range, a count bump
        let mut res: SyntaxElement = self.clone().into();
        loop {
            assert!(
                res.text_range().contains_range(range),
                "Bad range: node range {:?}, range {:?}",
                res.text_range(),
                range,
            );
            res = match &res {
                NodeOrToken::Token(_) => return res,
                NodeOrToken::Node(node) => match node.child_or_token_at_range(range) {
                    Some(it) => it,
                    None => return res,
                },
            };
        }
    }

    /// What stepping down by `child_or_token_at_range` from this node finds first of `kind`
    /// and `range`, when the range is not empty; `None` when it is, for the caller to step.
    pub(crate) fn flat_node_at_range(
        &self,
        range: TextRange,
        kind: SyntaxKind,
    ) -> Option<Option<SyntaxNode>> {
        (!range.is_empty()).then(|| flat_node_at_range(self.0.root, self.0.at, range, kind))
    }

    pub fn child_or_token_at_range(&self, range: TextRange) -> Option<SyntaxElement> {
        let (root, at) = inner(self.0.root, self.0.at);
        flat(&root).tree.child_at_range(at, range).map(|it| flat_element(root, it))
    }
}

impl SyntaxToken {
    #[inline]
    pub fn kind(&self) -> SyntaxKind {
        self.0.kind()
    }

    #[inline]
    pub fn text_range(&self) -> TextRange {
        self.0.text_range()
    }

    #[inline]
    pub fn index(&self) -> usize {
        self.0.index()
    }

    #[inline]
    pub fn text(&self) -> &str {
        self.0.tree().token_text(self.0.at)
    }

    #[inline]
    pub fn parent(&self) -> Option<SyntaxNode> {
        self.0.parent()
    }

    #[inline]
    pub fn ancestors(&self) -> impl Iterator<Item = SyntaxNode> {
        std::iter::successors(self.parent(), SyntaxNode::parent)
    }

    #[inline]
    pub fn tree_top(&self) -> SyntaxNode {
        SyntaxNode(flat_ref(owner(self.0.root), 0))
    }

    pub fn next_sibling_or_token(&self) -> Option<SyntaxElement> {
        self.0.next_sibling_or_token()
    }

    pub fn prev_sibling_or_token(&self) -> Option<SyntaxElement> {
        self.0.prev_sibling_or_token()
    }

    #[inline]
    pub fn siblings_with_tokens(
        &self,
        direction: Direction,
    ) -> impl Iterator<Item = SyntaxElement> {
        // clone: the walk's owned first cursor, a count bump
        let me: SyntaxElement = self.clone().into();
        iter::successors(Some(me), move |el| match direction {
            Direction::Next => el.next_sibling_or_token(),
            Direction::Prev => el.prev_sibling_or_token(),
        })
    }

    pub fn next_token(&self) -> Option<SyntaxToken> {
        match self.next_sibling_or_token() {
            Some(element) => element.first_token(),
            None => self
                .ancestors()
                .find_map(|it| it.next_sibling_or_token())
                .and_then(|element| element.first_token()),
        }
    }

    pub fn prev_token(&self) -> Option<SyntaxToken> {
        match self.prev_sibling_or_token() {
            Some(element) => element.last_token(),
            None => self
                .ancestors()
                .find_map(|it| it.prev_sibling_or_token())
                .and_then(|element| element.last_token()),
        }
    }
}

impl SyntaxElement {
    #[inline]
    pub fn text_range(&self) -> TextRange {
        match self {
            NodeOrToken::Node(it) => it.text_range(),
            NodeOrToken::Token(it) => it.text_range(),
        }
    }

    #[inline]
    pub fn index(&self) -> usize {
        match self {
            NodeOrToken::Node(it) => it.index(),
            NodeOrToken::Token(it) => it.index(),
        }
    }

    #[inline]
    pub fn kind(&self) -> SyntaxKind {
        match self {
            NodeOrToken::Node(it) => it.kind(),
            NodeOrToken::Token(it) => it.kind(),
        }
    }

    #[inline]
    pub fn parent(&self) -> Option<SyntaxNode> {
        match self {
            NodeOrToken::Node(it) => it.parent(),
            NodeOrToken::Token(it) => it.parent(),
        }
    }

    #[inline]
    pub fn ancestors(&self) -> impl Iterator<Item = SyntaxNode> {
        let first = match self {
            // clone: the walk's owned first cursor, a count bump
            NodeOrToken::Node(it) => Some(it.clone()),
            NodeOrToken::Token(it) => it.parent(),
        };
        iter::successors(first, SyntaxNode::parent)
    }

    #[inline]
    pub fn tree_top(&self) -> SyntaxNode {
        match self {
            NodeOrToken::Node(it) => it.tree_top(),
            NodeOrToken::Token(it) => it.tree_top(),
        }
    }

    pub fn first_token(&self) -> Option<SyntaxToken> {
        match self {
            NodeOrToken::Node(it) => it.first_token(),
            // clone: the owned token the signature returns, a count bump
            NodeOrToken::Token(it) => Some(it.clone()),
        }
    }

    pub fn last_token(&self) -> Option<SyntaxToken> {
        match self {
            NodeOrToken::Node(it) => it.last_token(),
            // clone: the owned token the signature returns, a count bump
            NodeOrToken::Token(it) => Some(it.clone()),
        }
    }

    pub fn next_sibling_or_token(&self) -> Option<SyntaxElement> {
        match self {
            NodeOrToken::Node(it) => it.next_sibling_or_token(),
            NodeOrToken::Token(it) => it.next_sibling_or_token(),
        }
    }

    pub fn prev_sibling_or_token(&self) -> Option<SyntaxElement> {
        match self {
            NodeOrToken::Node(it) => it.prev_sibling_or_token(),
            NodeOrToken::Token(it) => it.prev_sibling_or_token(),
        }
    }

    fn token_at_offset(&self, offset: TextSize) -> TokenAtOffset<SyntaxToken> {
        assert!(self.text_range().start() <= offset && offset <= self.text_range().end());
        match self {
            // clone: the owned token the signature returns, a count bump
            NodeOrToken::Token(token) => TokenAtOffset::Single(token.clone()),
            NodeOrToken::Node(node) => node.token_at_offset(offset),
        }
    }
}

// region: impls

// Identity semantics for hash & eq
impl PartialEq for SyntaxNode {
    #[inline]
    fn eq(&self, other: &SyntaxNode) -> bool {
        self.0.key() == other.0.key()
    }
}

impl Eq for SyntaxNode {}

impl Hash for SyntaxNode {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.key().hash(state);
    }
}

impl fmt::Debug for SyntaxNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyntaxNode")
            .field("kind", &self.kind())
            .field("text_range", &self.text_range())
            .finish()
    }
}

impl fmt::Display for SyntaxNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.preorder_with_tokens()
            .filter_map(|event| match event {
                WalkEvent::Enter(NodeOrToken::Token(token)) => Some(token),
                _ => None,
            })
            .try_for_each(|it| fmt::Display::fmt(&it, f))
    }
}

// Identity semantics for hash & eq
impl PartialEq for SyntaxToken {
    #[inline]
    fn eq(&self, other: &SyntaxToken) -> bool {
        self.0.key() == other.0.key()
    }
}

impl Eq for SyntaxToken {}

impl Hash for SyntaxToken {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.key().hash(state);
    }
}

impl fmt::Debug for SyntaxToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyntaxToken")
            .field("kind", &self.kind())
            .field("text_range", &self.text_range())
            .finish()
    }
}

impl fmt::Display for SyntaxToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.text(), f)
    }
}

impl From<SyntaxNode> for SyntaxElement {
    #[inline]
    fn from(node: SyntaxNode) -> SyntaxElement {
        NodeOrToken::Node(node)
    }
}

impl From<SyntaxToken> for SyntaxElement {
    #[inline]
    fn from(token: SyntaxToken) -> SyntaxElement {
        NodeOrToken::Token(token)
    }
}

// endregion

// region: iterators

#[derive(Clone, Debug)]
pub struct SyntaxNodeChildren {
    next: Option<SyntaxNode>,
}

impl SyntaxNodeChildren {
    fn new(parent: &SyntaxNode) -> SyntaxNodeChildren {
        SyntaxNodeChildren { next: parent.first_child() }
    }
}

impl Iterator for SyntaxNodeChildren {
    type Item = SyntaxNode;
    fn next(&mut self) -> Option<SyntaxNode> {
        self.next.take().map(|next| {
            self.next = next.next_sibling();
            next
        })
    }
}

/// The child nodes of a node whose kinds a predicate accepts, from
/// [`SyntaxNode::children_where`], read off the arrays: only those taken are made cursors.
#[derive(Clone, Debug)]
pub struct SyntaxNodeChildrenWhere {
    /// The node whose children these are (in its part when it is a lazy node), keeping
    /// `root` alive.
    _parent: SyntaxNode,
    root: ptr::NonNull<FlatRoot>,
    next: u32,
    end: u32,
}

impl SyntaxNodeChildrenWhere {
    /// The next child whose kind `pred` accepts.
    #[inline]
    pub fn next_where(&mut self, mut pred: impl FnMut(SyntaxKind) -> bool) -> Option<SyntaxNode> {
        let root = self.root;
        let tree = &flat(&root).tree;
        while self.next < self.end {
            let it = self.next;
            self.next = tree.end(it);
            if !tree.is_token(it) && pred(tree.kind(it)) {
                return Some(SyntaxNode(flat_ref(root, it)));
            }
        }
        None
    }
}

#[derive(Clone, Debug)]
pub struct SyntaxElementChildren {
    next: Option<SyntaxElement>,
}

impl SyntaxElementChildren {
    fn new(parent: &SyntaxNode) -> SyntaxElementChildren {
        SyntaxElementChildren { next: parent.first_child_or_token() }
    }
}

impl Iterator for SyntaxElementChildren {
    type Item = SyntaxElement;
    fn next(&mut self) -> Option<SyntaxElement> {
        self.next.take().map(|next| {
            self.next = next.next_sibling_or_token();
            next
        })
    }
}

/// A walk of a node's subtree in preorder. Each step is taken when asked for, after a
/// [`Preorder::skip_subtree`] of the one before: entering a lazy node never moves into it, so
/// skipping it leaves its part unparsed.
#[derive(Debug, Clone)]
pub struct Preorder {
    start: SyntaxNode,
    /// The event returned last, `None` before the first and after the last.
    last: Option<WalkEvent<SyntaxNode>>,
    done: bool,
    skip_subtree: bool,
}

impl Preorder {
    fn new(start: SyntaxNode) -> Preorder {
        Preorder { start, last: None, done: false, skip_subtree: false }
    }

    /// Leaves the node entered last without walking its subtree; after a leave, leaves the
    /// parent without walking the siblings after.
    pub fn skip_subtree(&mut self) {
        self.skip_subtree = true;
    }
}

impl Iterator for Preorder {
    type Item = WalkEvent<SyntaxNode>;

    fn next(&mut self) -> Option<WalkEvent<SyntaxNode>> {
        let skip = std::mem::take(&mut self.skip_subtree);
        let event = match self.last.take() {
            None if self.done => None,
            // clone: the walk keeps its start to stop at and returns entering it, a count bump
            None => Some(WalkEvent::Enter(self.start.clone())),
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
        self.done = event.is_none();
        // clone: the next step is taken from the event returned, a count bump
        self.last = event.clone();
        event
    }
}

/// [`Preorder`] with tokens, each step taken when asked for as there.
#[derive(Debug, Clone)]
pub struct PreorderWithTokens {
    start: SyntaxElement,
    /// The event returned last, `None` before the first and after the last.
    last: Option<WalkEvent<SyntaxElement>>,
    done: bool,
    skip_subtree: bool,
}

impl PreorderWithTokens {
    fn new(start: SyntaxNode) -> PreorderWithTokens {
        PreorderWithTokens { start: start.into(), last: None, done: false, skip_subtree: false }
    }

    /// As [`Preorder::skip_subtree`].
    pub fn skip_subtree(&mut self) {
        self.skip_subtree = true;
    }
}

impl Iterator for PreorderWithTokens {
    type Item = WalkEvent<SyntaxElement>;

    fn next(&mut self) -> Option<WalkEvent<SyntaxElement>> {
        let skip = std::mem::take(&mut self.skip_subtree);
        let event = match self.last.take() {
            None if self.done => None,
            // clone: the walk keeps its start to stop at and returns entering it, a count bump
            None => Some(WalkEvent::Enter(self.start.clone())),
            Some(WalkEvent::Enter(NodeOrToken::Node(node))) => {
                Some(match if skip { None } else { node.first_child_or_token() } {
                    Some(child) => WalkEvent::Enter(child),
                    None => WalkEvent::Leave(node.into()),
                })
            }
            Some(WalkEvent::Enter(token)) => Some(WalkEvent::Leave(token)),
            Some(WalkEvent::Leave(el)) if el == self.start => None,
            Some(WalkEvent::Leave(el)) => {
                match if skip { None } else { el.next_sibling_or_token() } {
                    Some(sibling) => Some(WalkEvent::Enter(sibling)),
                    None => el.parent().map(|it| WalkEvent::Leave(it.into())),
                }
            }
        };
        self.done = event.is_none();
        // clone: the next step is taken from the event returned, a count bump
        self.last = event.clone();
        event
    }
}
// endregion

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::SyntaxNode;
    use crate::{FlatTree, FlatTreeBuilder, SyntaxKind, Text, TextRange};

    const FILE: SyntaxKind = SyntaxKind(1);
    const BODY: SyntaxKind = SyntaxKind(2);
    const WORD: SyntaxKind = SyntaxKind(3);
    const STATEMENT: SyntaxKind = SyntaxKind(4);

    /// A body's part: the body, one statement, its text one word.
    fn expand(
        kind: SyntaxKind,
        source: &Text,
        range: TextRange,
        _: u32,
    ) -> (FlatTree, Vec<(TextRange, Box<str>)>) {
        let mut builder = FlatTreeBuilder::sourced_range(source.clone(), range);
        builder.start_node(kind);
        builder.start_node(STATEMENT);
        builder.token(WORD, &source[range]);
        builder.finish_node();
        builder.finish_node();
        (builder.finish(), Vec::new())
    }

    /// The counted roots: a cursor into a part keeps its file's root, and the part's, after
    /// every other cursor is gone, and the last cursor frees both (each root holds its tree's
    /// `Arc`, so the counts show which roots live).
    #[test]
    fn a_cursor_into_a_part_keeps_its_file_and_the_last_frees_both() {
        let mut builder = FlatTreeBuilder::sourced(String::from("ab{cd}").into());
        builder.start_node(FILE);
        builder.token(WORD, "ab");
        builder.lazy(BODY, 4);
        builder.finish_node();
        builder.expand_with(expand, 0);
        let tree = Arc::new(builder.finish());
        let root = SyntaxNode::new_flat_root(Arc::clone(&tree));
        let body = root.first_child().unwrap();
        assert_eq!((body.kind(), body.is_lazy()), (BODY, true));
        let statement = body.first_child().unwrap();
        let part = Arc::clone(tree.part(2));
        assert_eq!(Arc::strong_count(&part), 3, "the tree's, the part's root's and this one");
        drop((root, body));
        assert_eq!(statement.kind(), STATEMENT);
        assert_eq!(statement.text_range(), TextRange::new(2.into(), 6.into()));
        let up = statement.parent().unwrap();
        assert_eq!((up.kind(), up.parent().map(|it| it.kind())), (BODY, Some(FILE)));
        assert_eq!(up.first_child_token_text(), None, "the body's first child is a statement");
        assert_eq!(statement.first_child_token_text(), Some("{cd}"));
        drop(up);
        assert_eq!((Arc::strong_count(&tree), Arc::strong_count(&part)), (2, 3));
        drop(statement);
        assert_eq!((Arc::strong_count(&tree), Arc::strong_count(&part)), (1, 2));
    }
}
