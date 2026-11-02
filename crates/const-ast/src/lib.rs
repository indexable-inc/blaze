//! Typed views over a flat syntax tree, as `const fn`s: what a const macro reads its input with
//! (docs/future/const-macros.md, stage 2; #1441).
//!
//! A [`Tree`] is the arrays of `syntax-tree`'s flat tree (`crates/syntax-tree/src/flat.rs`):
//! every node and token in preorder, each with its kind, the number after its subtree, the
//! index of its span in the call's span table, and, for a token, its text. It is plain data
//! with no pointers but its slices, so a host can write it into the const evaluator's heap
//! once and hand a macro a reference to it.
//!
//! A macro never sees a kind's number: `SyntaxKind`s are numbered by the grammar and move when
//! it changes, so a `kind == N` in a macro would match the wrong node after a grammar bump.
//! It reads the tree through the views instead ([`Item::as_struct`], [`Struct::field_list`],
//! [`RecordField::ty`], [`Attr::path`], ...), ported one for one from `syntax::ast`'s
//! accessors, which change with the grammar here ([`kinds`]). The views are code the macro
//! reaches, so their hash is in every expansion's key.
//!
//! Everything a macro calls is a `const fn`, so blaze's const evaluator runs it; iterating
//! children is a cursor's `next` (a `while let`), since `Iterator` is not a const trait. The
//! cursors implement `Iterator` too, for code outside const. Every view is `Copy`, two words
//! (the tree and an element's number), and the subtree a view stands for is the elements
//! `index() .. index() + len()`.
//!
//! `tests` compares every view with `syntax::ast`'s accessor of the same name, on every node of
//! the files of real crates; `crates/hir-ty-mir`'s `const_ast` test runs the views in the
//! evaluator and compares what they read with the same code compiled.

#![no_std]
#![forbid(unsafe_code)]

mod kinds;
mod views;

#[cfg(test)]
mod tests;

pub use views::*;

/// The crate's files, by their path from its root: what blaze puts in a const macro's crate
/// graph, so that the views a macro reads its input with are the code that wrote the input.
#[cfg(feature = "source")]
pub const SOURCE: [(&str, &str); 3] = [
    ("lib.rs", include_str!("lib.rs")),
    ("kinds.rs", include_str!("kinds.rs")),
    ("views.rs", include_str!("views.rs")),
];

/// Set on the kind of an element that is a token. A node and a token can have the same
/// kind (`ERROR`), so whether an element is a token is said, not read from its kind.
pub const TOKEN: u16 = 1 << 15;

/// The span index of an element that has none of its own (a node).
pub const NO_SPAN: u32 = !0;

/// A flat syntax tree: element `i` is `kinds[i]` (with [`TOKEN`] set for a token), ends at
/// `ends[i]` (the number after its subtree), has the span `spans[i]` and, if it is a token,
/// the text `texts[i]`. Element 0 is the root.
///
/// The layout is `repr(C)` so that a host can write one into the evaluator's memory.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Tree<'a> {
    kinds: &'a [u16],
    ends: &'a [u32],
    spans: &'a [u32],
    texts: &'a [&'a str],
}

impl<'a> Tree<'a> {
    /// The tree of these arrays, which are as long as each other and not empty.
    ///
    /// # Panics
    ///
    /// When they are not.
    pub const fn new(
        kinds: &'a [u16],
        ends: &'a [u32],
        spans: &'a [u32],
        texts: &'a [&'a str],
    ) -> Tree<'a> {
        let len = kinds.len();
        if len == 0 || ends.len() != len || spans.len() != len || texts.len() != len {
            panic!("a flat tree's arrays are as long as each other and not empty");
        }
        Tree { kinds, ends, spans, texts }
    }

    /// How many nodes and tokens the tree has.
    pub const fn len(&self) -> u32 {
        self.kinds.len() as u32
    }

    /// Never: a tree has a root.
    pub const fn is_empty(&self) -> bool {
        self.kinds.len() == 0
    }

    /// The root, element 0.
    pub const fn root(&'a self) -> Node<'a> {
        Node { tree: self, at: 0 }
    }

    /// The item at the root, a macro's input: the root itself, or the first item of a file.
    pub const fn item(&'a self) -> Option<Item<'a>> {
        let root = self.root();
        match Item::cast(root) {
            Some(item) => Some(item),
            None => match SourceFile::cast(root) {
                Some(file) => file.items().next(),
                None => None,
            },
        }
    }

    #[inline]
    const fn kind(&self, at: u32) -> u16 {
        self.kinds[at as usize]
    }

    #[inline]
    const fn end(&self, at: u32) -> u32 {
        self.ends[at as usize]
    }
}

/// A node of a [`Tree`]: the tree and its element's number.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Node<'a> {
    tree: &'a Tree<'a>,
    at: u32,
}

impl<'a> Node<'a> {
    #[inline]
    const fn kind(self) -> u16 {
        self.tree.kind(self.at)
    }

    /// The tree the node is in.
    pub const fn tree(self) -> &'a Tree<'a> {
        self.tree
    }

    /// The element's number: its subtree is `index() .. index() + len()`.
    pub const fn index(self) -> u32 {
        self.at
    }

    /// How many elements its subtree has, itself included.
    pub const fn len(self) -> u32 {
        self.tree.end(self.at) - self.at
    }

    /// Never: a node's subtree has the node.
    pub const fn is_empty(self) -> bool {
        false
    }

    /// The node's children that are nodes, in order.
    pub const fn children(self) -> Children<'a> {
        Children { tree: self.tree, next: self.at + 1, end: self.tree.end(self.at) }
    }

    /// The tokens of the subtree, in order, without whitespace and comments: what a proc macro
    /// is given of it.
    pub const fn tokens(self) -> Tokens<'a> {
        Tokens { tree: self.tree, next: self.at + 1, end: self.tree.end(self.at) }
    }

    /// The first token of the subtree, trivia included.
    const fn first_token(self) -> Option<Token<'a>> {
        let end = self.tree.end(self.at);
        let mut at = self.at + 1;
        while at < end {
            if self.tree.kind(at) & TOKEN != 0 {
                return Some(Token { tree: self.tree, at });
            }
            at += 1;
        }
        None
    }

    /// The first child node of kind `kind` (`support::child`).
    const fn child(self, kind: u16) -> Option<Node<'a>> {
        let mut children = self.children();
        while let Some(child) = children.next() {
            if child.kind() == kind {
                return Some(child);
            }
        }
        None
    }

    /// The first token child of kind `kind` (`support::token`).
    const fn token(self, kind: u16) -> Option<Token<'a>> {
        let end = self.tree.end(self.at);
        let mut at = self.at + 1;
        while at < end {
            if self.tree.kind(at) == kind {
                return Some(Token { tree: self.tree, at });
            }
            at = self.tree.end(at);
        }
        None
    }
}

/// A token of a [`Tree`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Token<'a> {
    tree: &'a Tree<'a>,
    at: u32,
}

impl<'a> Token<'a> {
    /// The token's text.
    pub const fn text(self) -> &'a str {
        self.tree.texts[self.at as usize]
    }

    /// The index of its span in the call's span table.
    pub const fn span(self) -> u32 {
        self.tree.spans[self.at as usize]
    }

    /// The element's number.
    pub const fn index(self) -> u32 {
        self.at
    }

    /// Whether it is whitespace or a comment.
    pub const fn is_trivia(self) -> bool {
        let kind = self.tree.kind(self.at);
        kind == kinds::WHITESPACE || kind == kinds::COMMENT
    }
}

/// The child nodes of a node.
#[derive(Clone, Copy)]
pub struct Children<'a> {
    tree: &'a Tree<'a>,
    next: u32,
    end: u32,
}

impl<'a> Children<'a> {
    /// The next child node.
    #[allow(clippy::should_implement_trait)]
    pub const fn next(&mut self) -> Option<Node<'a>> {
        while self.next < self.end {
            let at = self.next;
            self.next = self.tree.end(at);
            if self.tree.kind(at) & TOKEN == 0 {
                return Some(Node { tree: self.tree, at });
            }
        }
        None
    }
}

impl<'a> Iterator for Children<'a> {
    type Item = Node<'a>;
    fn next(&mut self) -> Option<Node<'a>> {
        Children::next(self)
    }
}

/// The tokens of a subtree, without trivia.
#[derive(Clone, Copy)]
pub struct Tokens<'a> {
    tree: &'a Tree<'a>,
    next: u32,
    end: u32,
}

impl<'a> Tokens<'a> {
    /// The next token that is not trivia.
    #[allow(clippy::should_implement_trait)]
    pub const fn next(&mut self) -> Option<Token<'a>> {
        while self.next < self.end {
            let at = self.next;
            self.next += 1;
            if self.tree.kind(at) & TOKEN != 0 {
                let token = Token { tree: self.tree, at };
                if !token.is_trivia() {
                    return Some(token);
                }
            }
        }
        None
    }
}

impl<'a> Iterator for Tokens<'a> {
    type Item = Token<'a>;
    fn next(&mut self) -> Option<Token<'a>> {
        Tokens::next(self)
    }
}

/// Whether two strings are equal, as a `const fn` (`==` on `str` is a trait's).
pub const fn str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}
