//! Stateful iteration over token trees.
//!
//! We use this as the source of tokens for parser.
use crate::{DelimiterKind, Leaf, Subtree, TokenTree, TokenTreesView};

pub struct Cursor<'a> {
    origin: TokenTreesView<'a>,
    /// The token tree the cursor is at, `origin.end` once past the last.
    at: u32,
    /// The subtrees the cursor is in, the innermost last: each one's index, and the index after
    /// its last token tree.
    subtrees_stack: Vec<(u32, u32)>,
}

impl<'a> Cursor<'a> {
    pub fn new(origin: TokenTreesView<'a>) -> Self {
        // alloc: one buffer per walk, at its first subtree, as deep as the nesting.
        Self { origin, at: origin.start, subtrees_stack: Vec::new() }
    }

    /// Check whether it is eof
    pub fn eof(&self) -> bool {
        self.at >= self.origin.end && self.subtrees_stack.is_empty()
    }

    pub fn is_root(&self) -> bool {
        self.subtrees_stack.is_empty()
    }

    /// Whether the cursor is past the last token tree of the subtree it is in.
    #[inline]
    fn at_subtree_end(&self) -> bool {
        self.subtrees_stack.last().is_some_and(|&(_, end)| end == self.at)
    }

    pub(crate) fn remaining(&self) -> TokenTreesView<'a> {
        TokenTreesView { start: self.at, ..self.origin }
    }

    pub fn end(&mut self) -> Subtree {
        let &(subtree, end) =
            self.subtrees_stack.last().expect("called `Cursor::end()` without an open subtree");
        assert_eq!(end, self.at, "called `Cursor::end()` without finishing a subtree");
        self.subtrees_stack.pop();
        self.origin.tree.subtree_at(subtree)
    }

    /// Returns the `TokenTree` at the cursor if it is not at the end of a subtree.
    pub fn token_tree(&self) -> Option<TokenTree> {
        if self.at_subtree_end() || self.at >= self.origin.end {
            return None;
        }
        Some(self.origin.tree.token_tree_at(self.at))
    }

    fn advance(&mut self) {
        if self.at >= self.origin.end {
            return;
        }
        let tree = self.origin.tree;
        if tree.is_subtree(self.at) {
            self.subtrees_stack.push((self.at, self.at + 1 + tree.subtree_len(self.at)));
        }
        self.at += 1;
    }

    /// Bump the cursor, and enters a subtree if it is on one.
    pub fn bump(&mut self) {
        assert!(!self.at_subtree_end(), "called `Cursor::bump()` when at the end of a subtree");
        self.advance();
    }

    pub fn bump_or_end(&mut self) {
        if self.at_subtree_end() {
            self.subtrees_stack.pop();
            return;
        }
        self.advance();
    }

    /// Steps out of the token trees without delimiters that end here, which a parser that
    /// counts tokens does not see.
    pub fn leave_invisible(&mut self) {
        while let Some(&(subtree, end)) = self.subtrees_stack.last()
            && end == self.at
            && self.origin.tree.delimiter_kind_at(subtree) == DelimiterKind::Invisible
        {
            self.subtrees_stack.pop();
        }
    }

    /// Steps over the starts and ends of token trees without delimiters, up to the next token
    /// that a parser sees.
    pub fn skip_invisible(&mut self) {
        loop {
            self.leave_invisible();
            let tree = self.origin.tree;
            if !self.at_subtree_end()
                && self.at < self.origin.end
                && tree.is_subtree(self.at)
                && tree.delimiter_kind_at(self.at) == DelimiterKind::Invisible
            {
                self.advance();
            } else {
                break;
            }
        }
    }

    pub fn peek_two_leaves(&self) -> Option<[Leaf; 2]> {
        if let Some(&(_, end)) = self.subtrees_stack.last()
            && (end == self.at || end == self.at + 1)
        {
            return None;
        }
        let tree = self.origin.tree;
        if self.at + 1 >= self.origin.end || tree.is_subtree(self.at) || tree.is_subtree(self.at + 1)
        {
            return None;
        }
        Some([tree.leaf_at(self.at), tree.leaf_at(self.at + 1)])
    }

    pub fn crossed(&self) -> TokenTreesView<'a> {
        assert!(self.is_root());
        TokenTreesView { end: self.at, ..self.origin }
    }
}
