//! THE ONE READING of where a syntactic element ends in this crate (end-state.md D167(d): one
//! walk per syntax). An element's extent is what the parser (crates/parser, the grammar every
//! fragment matcher here already parses with) takes for it, never a count of commas, `<` or `>`
//! over tokens: `<` and `>` delimit no token tree, and a comma sits inside `f::<A, B>()`,
//! `|a, b| a`, `HashMap<K, V>` and `T = HashMap<K, V>` without ending the element it is in.
//! Two hand walks that did count them gave today's two bugs (item.rs `element_end` on reth's
//! `ChainHardforks`, t1545; builtin.rs `cfg_select!` on a turbofish, t1800).
//!
//! Two readings, both from one parse:
//!
//! - [`Words::extent`]: where the element a parser entry point reads from a token ends (an
//!   argument expression, an attribute's meta), as a token index of the stream.
//! - [`item`]: the item parsed once into a [`Tree`], each node with its token range: what a
//!   derive's item is read from (adt.rs) and how its input is cfg-stripped, by removing whole
//!   parsed elements (item.rs `derive_input`).
//!
//! A parse that refuses is a typed [`ExtentError`] naming the innermost element it was in
//! (rule 3, 19): never an element cut short or quietly kept.
//!
//! The guard (rule 54, 73): astlog's `expand-hand-extent` (tools/astlog/rules) refuses a `','`,
//! `'<'` or `'>'` character literal in this crate's sources outside this file; a site that
//! matches one for another reason (the macro language's own separators, a delimiter it writes)
//! says why on an `astlog-ignore` line.

use core::{fmt, ops::Range};

use blaze_own::{AVec, Alloc, Vec};
use parser::{PrefixEntryPoint, Step, SyntaxKind};

use crate::{
    body::{ParserWords, parser_words},
    tokens::{Delim, Token, TokenKind, TokenStream, index},
};

/// A syntactic element: what an extent is of, or what a refusal names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Extent {
    Item,
    Expr,
    /// An attribute's meta: `path`, `path(..)`, `path = expr`, `cfg(..)`, `cfg_attr(..)`.
    Meta,
    /// A record or tuple field, its attributes, visibility, type and default with it.
    Field,
    Variant,
    /// A lifetime, type or const parameter, its attributes, bounds and default with it.
    GenericParam,
    WherePredicate,
    Attr,
    Arm,
    Stmt,
}

impl Extent {
    /// Its name in a message.
    pub fn name(self) -> &'static str {
        match self {
            Extent::Item => "an item",
            Extent::Expr => "an expression",
            Extent::Meta => "an attribute's meta",
            Extent::Field => "a field",
            Extent::Variant => "a variant",
            Extent::GenericParam => "a generic parameter",
            Extent::WherePredicate => "a `where` predicate",
            Extent::Attr => "an attribute",
            Extent::Arm => "a match arm",
            Extent::Stmt => "a statement",
        }
    }

    /// A refusal of it, whole, for a writer that takes a static message (derive.rs `refuse`).
    pub fn refused(self) -> &'static str {
        match self {
            Extent::Item => "the item does not parse",
            Extent::Expr => "an expression of the item does not parse",
            Extent::Meta => "an attribute's meta does not parse",
            Extent::Field => "a field of the item does not parse",
            Extent::Variant => "a variant of the item does not parse",
            Extent::GenericParam => "a generic parameter of the item does not parse",
            Extent::WherePredicate => "a `where` predicate of the item does not parse",
            Extent::Attr => "an attribute of the item does not parse",
            Extent::Arm => "a match arm of the item does not parse",
            Extent::Stmt => "a statement of the item does not parse",
        }
    }

    /// The element a parser node is, when it is one a refusal names.
    fn of(kind: SyntaxKind) -> Option<Extent> {
        Some(match kind {
            SyntaxKind::RECORD_FIELD | SyntaxKind::TUPLE_FIELD => Extent::Field,
            SyntaxKind::VARIANT => Extent::Variant,
            SyntaxKind::TYPE_PARAM | SyntaxKind::LIFETIME_PARAM | SyntaxKind::CONST_PARAM => Extent::GenericParam,
            SyntaxKind::WHERE_PRED => Extent::WherePredicate,
            SyntaxKind::ATTR => Extent::Attr,
            SyntaxKind::MATCH_ARM => Extent::Arm,
            SyntaxKind::LET_STMT | SyntaxKind::EXPR_STMT => Extent::Stmt,
            _ => return None,
        })
    }
}

/// A parser entry point an extent is read with: the elements that start a reading.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Prefix {
    Item,
    Expr,
    Meta,
}

impl Prefix {
    fn entry(self) -> PrefixEntryPoint {
        match self {
            Prefix::Item => PrefixEntryPoint::Item,
            Prefix::Expr => PrefixEntryPoint::Expr,
            Prefix::Meta => PrefixEntryPoint::MetaItem,
        }
    }

    fn extent(self) -> Extent {
        match self {
            Prefix::Item => Extent::Item,
            Prefix::Expr => Extent::Expr,
            Prefix::Meta => Extent::Meta,
        }
    }
}

/// Why an element has no extent.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExtentWhy {
    /// The parser refuses the tokens there (nothing of the element is there, or it is not
    /// Rust's syntax).
    Refused,
    /// The element ends before the tokens it is to take all of: tokens left after an item, or
    /// after an attribute's meta before its `]`.
    Trailing,
    /// Two elements of a list with no `,` between them.
    Unseparated,
    /// The element starts or ends inside an invisible group (a `macro_rules!` fragment) that it
    /// does not take whole.
    Unbalanced,
}

/// An element with no extent: which, why, and the token of the stream read where it stops.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ExtentError {
    pub what: Extent,
    pub why: ExtentWhy,
    pub token: u32,
}

impl fmt::Display for ExtentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let why = match self.why {
            ExtentWhy::Refused => "the parser refuses it",
            ExtentWhy::Trailing => "tokens are left after it",
            ExtentWhy::Unseparated => "no `,` follows it",
            ExtentWhy::Unbalanced => "it cuts an invisible group",
        };
        write!(f, "{} does not parse: {why}, at token {}", self.what.name(), self.token)
    }
}

/// What follows an element ([`Words::after`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum After {
    End,
    Comma,
    Other,
}

/// No parent: the root's.
const ROOT: u32 = u32::MAX;

/// One node or token of a [`Tree`], in preorder.
#[derive(Clone, Copy, Debug)]
struct Node {
    kind: SyntaxKind,
    /// The node's tokens of the stream read: a node's balanced, an invisible group's
    /// delimiters taken only when it holds the whole group ([`Words::node_range`]); a token's
    /// without the invisible delimiters around it.
    start: u32,
    end: u32,
    parent: u32,
    /// The index past the node's subtree: the next sibling's, or the parent's next.
    past: u32,
    /// A token, not a node.
    leaf: bool,
}

/// An element parsed once, its nodes and tokens in preorder: what the readers of a derive's
/// item take ranges from.
pub(crate) struct Tree<'a> {
    nodes: AVec<'a, Node>,
    /// The token past the element.
    end: usize,
}

impl Tree<'_> {
    pub(crate) fn len(&self) -> usize {
        self.nodes.len()
    }

    pub(crate) fn kind(&self, node: usize) -> SyntaxKind {
        self.nodes[node].kind
    }

    /// The node's tokens.
    pub(crate) fn range(&self, node: usize) -> Range<usize> {
        self.nodes[node].start as usize..self.nodes[node].end as usize
    }

    pub(crate) fn parent(&self, node: usize) -> Option<usize> {
        let parent = self.nodes[node].parent;
        (parent != ROOT).then_some(parent as usize)
    }

    /// The index past the node's subtree.
    pub(crate) fn past(&self, node: usize) -> usize {
        self.nodes[node].past as usize
    }

    /// The token past the element.
    pub(crate) fn end(&self) -> usize {
        self.end
    }

    /// The node's children, nodes and tokens, in order.
    pub(crate) fn children(&self, node: usize) -> impl Iterator<Item = usize> + '_ {
        let past = self.past(node);
        let mut at = node + 1;
        core::iter::from_fn(move || {
            (at < past).then(|| {
                let child = at;
                at = self.past(child);
                child
            })
        })
    }

    /// Whether the entry is a token, not a node.
    pub(crate) fn is_token(&self, node: usize) -> bool {
        self.nodes[node].leaf
    }

    /// The node's first child of `kind`.
    pub(crate) fn child(&self, node: usize, kind: SyntaxKind) -> Option<usize> {
        self.children(node).find(|&child| self.kind(child) == kind)
    }

    /// The node's descendants, nodes and tokens, in preorder.
    pub(crate) fn descendants(&self, node: usize) -> Range<usize> {
        node + 1..self.past(node)
    }

    /// The first token of the node's first token: a name's, a lifetime's `'`; never an
    /// invisible delimiter of a fragment the node is all of (a leaf's range has none).
    pub(crate) fn token(&self, node: usize) -> usize {
        match (node..self.past(node)).find(|&n| self.nodes[n].leaf) {
            Some(leaf) => self.nodes[leaf].start as usize,
            None => self.nodes[node].start as usize,
        }
    }

    /// The tokens that removing the node from its list removes: the node, and the `,` that
    /// ends it when one does (the last element of a list without a trailing comma keeps the
    /// comma before it, as rustc's cfg-stripping leaves it).
    pub(crate) fn removal(&self, node: usize) -> Range<usize> {
        let range = self.range(node);
        let next = self.past(node);
        match self.nodes.get(next) {
            Some(after) if after.kind == SyntaxKind::COMMA && after.parent == self.nodes[node].parent => {
                range.start..after.end as usize
            }
            _ => range,
        }
    }
}

/// A stream's parser words: what every extent of it is read from, converted once.
pub(crate) struct Words<'s, 'a> {
    tokens: &'s [Token],
    words: ParserWords<'a>,
    /// Whether the stream holds an invisible group at all: else no range needs closing.
    invisible: bool,
}

impl<'s, 'a> Words<'s, 'a> {
    pub(crate) fn new(stream: &'s TokenStream<'_>, alloc: Alloc<'a>) -> Words<'s, 'a> {
        let tokens = stream.tokens();
        let invisible = tokens.iter().any(|t| t.kind == TokenKind::Open(Delim::Invisible));
        Words { tokens, words: parser_words(stream, alloc), invisible }
    }

    /// The word the token `at` starts (the next one, for an invisible delimiter).
    fn word_of(&self, at: usize) -> usize {
        self.words.word_at[at] as usize
    }

    /// The first token whose word is `word` or later.
    fn token_at(&self, word: usize) -> usize {
        self.words.word_at.partition_point(|&w| (w as usize) < word).min(self.tokens.len())
    }

    fn is(&self, at: usize, kind: TokenKind) -> bool {
        self.tokens.get(at).is_some_and(|t| t.kind == kind)
    }

    /// `start..end` with each invisible group it opens closed: `None` when it closes one it
    /// does not open, or opens one that does not close right after it.
    fn closed(&self, start: usize, mut end: usize) -> Option<usize> {
        if !self.invisible {
            return Some(end);
        }
        let mut depth = 0u32;
        for t in &self.tokens[start..end] {
            match t.kind {
                TokenKind::Open(Delim::Invisible) => depth += 1,
                TokenKind::Close(Delim::Invisible) => depth = depth.checked_sub(1)?,
                _ => {}
            }
        }
        while depth > 0 && self.is(end, TokenKind::Close(Delim::Invisible)) {
            end += 1;
            depth -= 1;
        }
        (depth == 0).then_some(end)
    }

    /// Where the element `prefix` reads from the token `from` ends: the token past it, the
    /// invisible groups it opens closed.
    pub(crate) fn extent(&self, prefix: Prefix, from: usize, alloc: Alloc<'_>) -> Result<usize, ExtentError> {
        Ok(self.tree(prefix, from, alloc)?.end)
    }

    /// What follows an element that ends at `end`: the stream's end, the `,` of a list, or
    /// another token.
    pub(crate) fn after(&self, end: usize) -> After {
        match self.tokens.get(end).map(|t| t.kind) {
            None => After::End,
            Some(TokenKind::Punct { char: ',', .. }) => After::Comma,
            Some(_) => After::Other,
        }
    }

    /// The elements `prefix` of the `,`-separated list that is all of `range`, a trailing comma
    /// taken: each one's tokens, its invisible groups whole.
    pub(crate) fn list<'b>(
        &self,
        prefix: Prefix,
        range: Range<usize>,
        alloc: Alloc<'b>,
    ) -> Result<AVec<'b, Range<usize>>, ExtentError> {
        let mut out = Vec::new_in(alloc);
        let mut at = range.start;
        while at < range.end {
            let end = self.extent(prefix, at, alloc)?;
            if end > range.end {
                return Err(ExtentError { what: prefix.extent(), why: ExtentWhy::Trailing, token: index(range.end) });
            }
            out.push(at..end);
            if end == range.end {
                break;
            }
            if !matches!(self.tokens[end].kind, TokenKind::Punct { char: ',', .. }) {
                return Err(ExtentError { what: prefix.extent(), why: ExtentWhy::Unseparated, token: index(end) });
            }
            at = end + 1;
        }
        Ok(out)
    }

    /// The element `prefix` reads from the token `from`, parsed into a [`Tree`] in `alloc`.
    pub(crate) fn tree<'b>(&self, prefix: Prefix, from: usize, alloc: Alloc<'b>) -> Result<Tree<'b>, ExtentError> {
        let base = self.word_of(from);
        let output = prefix.entry().parse(parser::Tokens::new(&self.words.words[base..], true));
        let mut nodes: AVec<'b, Node> = Vec::new_in(alloc);
        let mut open: AVec<'_, u32> = Vec::new_in(alloc);
        let mut word = base;
        // Word ranges first; tokens once the parse is whole.
        fn leaf(nodes: &mut AVec<'_, Node>, parent: u32, kind: SyntaxKind, word: usize, n: usize) {
            let past = index(nodes.len() + 1);
            nodes.push(Node { kind, start: index(word), end: index(word + n), parent, past, leaf: true });
        }
        for step in output.iter() {
            match step {
                Step::Enter { kind } => {
                    let parent = open.last().copied().unwrap_or(ROOT);
                    open.push(index(nodes.len()));
                    nodes.push(Node { kind, start: index(word), end: index(word), parent, past: 0, leaf: false });
                }
                Step::Exit => {
                    let Some(node) = open.pop() else {
                        return Err(ExtentError { what: prefix.extent(), why: ExtentWhy::Refused, token: index(self.token_at(word)) });
                    };
                    let past = index(nodes.len());
                    let node = &mut nodes[node as usize];
                    (node.end, node.past) = (index(word), past);
                }
                Step::Token { kind, n_input_tokens } => {
                    let parent = open.last().copied().unwrap_or(ROOT);
                    leaf(&mut nodes, parent, kind, word, n_input_tokens as usize);
                    word += n_input_tokens as usize;
                }
                Step::FloatSplit { .. } => {
                    let parent = open.last().copied().unwrap_or(ROOT);
                    leaf(&mut nodes, parent, SyntaxKind::FLOAT_NUMBER, word, 1);
                    word += 1;
                }
                Step::Error { .. } => {
                    let what = open.iter().rev().find_map(|&n| Extent::of(nodes[n as usize].kind)).unwrap_or(prefix.extent());
                    return Err(ExtentError { what, why: ExtentWhy::Refused, token: index(self.token_at(word).max(from)) });
                }
            }
        }
        if word == base || !open.is_empty() {
            return Err(ExtentError { what: prefix.extent(), why: ExtentWhy::Refused, token: index(from) });
        }
        for at in 0..nodes.len() {
            let node = nodes[at];
            let range = match node.leaf {
                // A token: without the invisible delimiters before it (a lifetime from its `'`).
                true => {
                    let mut start = self.token_at(node.start as usize).max(from);
                    let end = self.token_at(node.end as usize).max(start);
                    while start < end && self.is_invisible(start) {
                        start += 1;
                    }
                    Some(start..end)
                }
                false => self.node_range(node.start as usize, node.end as usize, from),
            };
            let Some(range) = range else {
                // The innermost element the node is part of, itself included.
                let mut owner = Some(at);
                let what = loop {
                    let Some(n) = owner else { break prefix.extent() };
                    if let Some(what) = Extent::of(nodes[n].kind) {
                        break what;
                    }
                    owner = (nodes[n].parent != ROOT).then_some(nodes[n].parent as usize);
                };
                let token = self.token_at(node.start as usize).max(from);
                return Err(ExtentError { what, why: ExtentWhy::Unbalanced, token: index(token) });
            };
            (nodes[at].start, nodes[at].end) = (index(range.start), index(range.end));
        }
        let start = from;
        let end = self.token_at(word).max(start);
        let end = self
            .closed(start, end)
            .ok_or(ExtentError { what: prefix.extent(), why: ExtentWhy::Unbalanced, token: index(start) })?;
        Ok(Tree { nodes, end })
    }

    /// The tokens of the node whose words are `start..end`, none before `from`. An invisible
    /// group (a fragment a macro transcribed, which rustc reads as one parsed unit) is the
    /// node's whole or not at all: the range runs from the node's first token to its last, and
    /// takes a group's delimiters only when it holds every token of that group. So a node
    /// within a fragment (the path `concat` of the expression `concat!(..)` that is all of a
    /// `$msg:expr`) stays within it, and one that is all of it, or goes on past it, takes it
    /// whole. `None` when the node cuts a group: it starts inside one and ends past it with
    /// other tokens of the group before its own (or the reverse). The ranges are over the
    /// stream's own tokens, never a flattened copy: each keeps its marks, and an output cut
    /// from them keeps the groups that make a fragment one operand.
    fn node_range(&self, start: usize, end: usize, from: usize) -> Option<Range<usize>> {
        let invisible_open = TokenKind::Open(Delim::Invisible);
        let invisible_close = TokenKind::Close(Delim::Invisible);
        // The first token at or past the first word: the delimiters before its token are the
        // groups' ends and starts around it, taken back below as the node holds them.
        let mut s = self.token_at(start).max(from);
        let mut e = self.token_at(end).max(s);
        while s < e && self.is_invisible(s) {
            s += 1;
        }
        // Inside: the groups that close in it unopened, and those that open in it unclosed.
        let (mut depth, mut lowest) = (0i64, 0i64);
        for t in &self.tokens[s..e] {
            match t.kind {
                kind if kind == invisible_open => depth += 1,
                kind if kind == invisible_close => {
                    depth -= 1;
                    lowest = lowest.min(depth);
                }
                _ => {}
            }
        }
        // A group closed inside it opens right before its first token, or it is cut.
        for _ in 0..-lowest {
            if s > from && self.is(s - 1, invisible_open) {
                s -= 1;
            } else {
                return None;
            }
        }
        // A group opened inside it closes right after its last token, or it is cut.
        for _ in 0..depth - lowest {
            if self.is(e, invisible_close) {
                e += 1;
            } else {
                return None;
            }
        }
        // Balanced now: each group right around it is all its tokens, so it is the node's.
        while s > from && self.is(s - 1, invisible_open) && self.is(e, invisible_close) {
            s -= 1;
            e += 1;
        }
        Some(s..e)
    }

    fn is_invisible(&self, at: usize) -> bool {
        self.tokens.get(at).is_some_and(|t| matches!(t.kind, TokenKind::Open(Delim::Invisible) | TokenKind::Close(Delim::Invisible)))
    }
}

/// The meta of the attribute node `attr` of `tree`, a parse of `stream` (`cfg(x)` of
/// `#[cfg(x)]`): its tokens without the invisible groups that are all of it, as `#[$attr]`
/// writes a `$attr:meta` fragment; empty at the attribute's end when it holds no meta.
pub(crate) fn meta_of(stream: &TokenStream<'_>, tree: &Tree<'_>, attr: usize) -> Range<usize> {
    let Some(meta) = tree.children(attr).find(|&c| !tree.is_token(c)) else {
        let end = tree.range(attr).end;
        return end..end;
    };
    let mut range = tree.range(meta);
    while range.len() >= 2
        && stream.tokens()[range.start].kind == TokenKind::Open(Delim::Invisible)
        && stream.close_of(range.start) == Some(range.end - 1)
    {
        range = range.start + 1..range.end - 1;
    }
    range
}

/// The item that is all of `stream`, parsed once into a [`Tree`] in `alloc`: its root is node
/// 0, and tokens left after it are refused.
pub(crate) fn item<'b>(stream: &TokenStream<'_>, alloc: Alloc<'b>) -> Result<Tree<'b>, ExtentError> {
    let words = Words::new(stream, alloc);
    let tree = words.tree(Prefix::Item, 0, alloc)?;
    if tree.end != stream.len() {
        return Err(ExtentError { what: Extent::Item, why: ExtentWhy::Trailing, token: index(tree.end) });
    }
    Ok(tree)
}
