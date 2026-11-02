//! The parser's words of a tree's tokens, made once and kept in the tree, so that parsing the
//! tree or any view of it reads them where they are and converts nothing
//! (docs/v2/plan/lanes/frontend/tokens.md, "Item 4").
//!
//! `tt` does not know what a word means: whoever makes them (`syntax_bridge`) does, from the
//! tokens [`Tree::for_each_slotted_token`] gives. The tree only keeps them and finds a
//! view's.

use std::{fmt, hash::Hash, sync::OnceLock};

use span::{Edition, SyntaxContext};

use super::{Node, SUBTREE, Tree};
use crate::{DelimiterKind, TokenTreesView};

/// A tree's tokens as the parser's words, in the parser's order: an open and a close for each
/// visible delimiter, one word a leaf, one word for the `'` of a lifetime and the ident after it,
/// and none for an invisible delimiter.
#[derive(Debug)]
pub struct TreeWords {
    /// The words, then whatever their maker puts after the last (the parser's end padding).
    words: Box<[u32]>,
    /// Per slot ([`Tree::token_slots`]), the index of its token's word. The ident of a
    /// lifetime has its `'`'s; a token without a word (an invisible delimiter) the next word's.
    at: Box<[u32]>,
    /// Sorted: each word that starts a fragment, with the node of the invisible group whose
    /// opening made it one. A view counts it only when the group is in the view, as a conversion
    /// of the view alone would: a view of the group's token trees, or one after an empty group,
    /// starts no fragment there. Only a view's first word can have its group outside the view.
    fragment_starts: Box<[(u32, u32)]>,
    /// The edition each of the tree's contexts was given when the words were made. A word holds
    /// its token's edition, so the words are of these answers, and a reader whose answers
    /// differ makes its own.
    editions: Box<[(SyntaxContext, Edition)]>,
}

impl TreeWords {
    /// The words of `tree`, as described on [`TreeWords`].
    pub fn new(
        tree: &Tree,
        words: Box<[u32]>,
        at: Box<[u32]>,
        fragment_starts: Box<[(u32, u32)]>,
        editions: Box<[(SyntaxContext, Edition)]>,
    ) -> TreeWords {
        assert_eq!(at.len(), tree.token_slots(), "one index a token slot");
        assert!(at.iter().all(|&it| it as usize <= words.len()), "indices into the words");
        assert!(fragment_starts.is_sorted(), "fragment starts in order");
        TreeWords { words, at, fragment_starts, editions }
    }

    /// The edition each context of the tree was given when the words were made.
    #[inline]
    pub fn editions(&self) -> &[(SyntaxContext, Edition)] {
        &self.editions
    }

    /// All the words, the tree's top delimiters' included when they are visible, then what
    /// their maker put after them.
    #[inline]
    pub fn words(&self) -> &[u32] {
        &self.words
    }
}

/// A view's words: its tree's from the view's first on, of which the view's are the first
/// `len`, and whether a fragment start on the first counts. What follows the view's words
/// tells the parser's input whether it can read them in place (only its end padding follows,
/// as for a view that ends where its tree does) or must copy them.
#[derive(Clone, Copy, Debug)]
pub struct ViewWords<'w> {
    pub rest: &'w [u32],
    pub len: usize,
    pub first_fragment_counts: bool,
}

impl<'w> ViewWords<'w> {
    /// The view's words.
    #[inline]
    pub fn words(self) -> &'w [u32] {
        &self.rest[..self.len]
    }
}

impl<'a> TokenTreesView<'a> {
    /// The tree the view is of.
    #[inline]
    pub fn tree(&self) -> &'a Tree {
        self.tree
    }

    /// The words the view's tree keeps ([`Tree::words`], made by `make` the first time).
    #[inline]
    pub fn tree_words(&self, make: impl FnOnce(&Tree) -> TreeWords) -> &'a TreeWords {
        self.tree.words(make)
    }

    /// The words of the view's tokens, from `words`, which are of the view's tree: those it
    /// keeps ([`TokenTreesView::tree_words`]) or others made of it. Steps through the view's
    /// top-level token trees to find its last, so costs one load a top-level token tree.
    pub fn words<'w>(&self, words: &'w TreeWords) -> ViewWords<'w> {
        let tree = self.tree;
        assert_eq!(words.at.len(), tree.token_slots(), "words of another tree");
        let nodes = tree.nodes();
        let mut at = self.start;
        let mut last = None;
        while at < self.end {
            last = Some(at);
            let node = Node::from_row(nodes[at as usize]);
            at += 1 + if node.tag() == SUBTREE { node.data } else { 0 };
        }
        let Some(last) = last else {
            return ViewWords { rest: &[], len: 0, first_fragment_counts: false };
        };
        let first = words.at[self.start as usize];
        let node = tree.node(last);
        let end = match node.tag() {
            SUBTREE => {
                let visible = node.delimiter_kind() != DelimiterKind::Invisible;
                words.at[tree.close_slot(last) as usize] + u32::from(visible)
            }
            _ => words.at[last as usize] + 1,
        };
        let first_fragment_counts = match words
            .fragment_starts
            .binary_search_by_key(&first, |&(word, _)| word)
        {
            Ok(found) => words.fragment_starts[found].1 >= self.start,
            Err(_) => true,
        };
        ViewWords {
            rest: &words.words[first as usize..],
            len: (end - first) as usize,
            first_fragment_counts,
        }
    }
}

impl Tree {
    /// The parser's words of the tree's tokens: `make`'s, the first time any are asked for.
    #[inline]
    pub fn words(&self, make: impl FnOnce(&Tree) -> TreeWords) -> &TreeWords {
        debug_assert!(self.keeps_words(), "a task's tree keeps no words");
        self.words.0.get_or_init(|| make(self))
    }
}

/// The words a tree keeps once made. They follow from its tokens and the editions of their
/// contexts, which the words record ([`TreeWords::editions`]), so they take no part in its
/// equality or hash, and a copy of the tree makes its own again.
#[derive(Default)]
pub(super) struct WordsCache(OnceLock<TreeWords>);

impl Clone for WordsCache {
    fn clone(&self) -> WordsCache {
        WordsCache::default()
    }
}

impl PartialEq for WordsCache {
    fn eq(&self, _: &WordsCache) -> bool {
        true
    }
}

impl Eq for WordsCache {}

impl Hash for WordsCache {
    fn hash<H: std::hash::Hasher>(&self, _: &mut H) {}
}

impl fmt::Debug for WordsCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.0.get().is_some() { "words" } else { "no words yet" })
    }
}
