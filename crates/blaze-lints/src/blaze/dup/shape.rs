//! The shape of code: a syntax tree read as a run of tokens with every local numbered by where it
//! is first used, so two pieces of code that differ only in what their locals are called have
//! one shape, while a different literal, path, field, method or type makes another (Baker's
//! parameterized matching, as Deckard's and NiCad's normalisers read it). Pure over a tree:
//! nothing here reads the database, so the tests parse text and check it alone.
//!
//! A local is a name a pattern of the item binds (its parameters, `let`s, closures, `for`s and
//! arms) that is not capitalised (`None`, a unit struct and a const in a pattern parse as one).
//! A use is a one-segment path naming it, a field shorthand, or the same identifier inside a
//! macro call's tokens. Shadowing is one local: `let x = 1; let x = x + 1;` renamed to
//! `let a = 1; let b = a + 1;` is another shape, a clone this misses, never a false match.
//! Nested items, attributes and comments are not part of a shape; nor, for the item read, its
//! own name and visibility.

use std::{
    alloc::Global,
    hash::{BuildHasher as _, Hash as _, Hasher as _},
};

use blaze_hash::{FixedHasher, FixedState, HashMap};
use syntax::{
    NodeOrToken, SyntaxKind, TextRange, WalkEvent,
    view::{self as ast, AstNode as _},
};

/// What a token is to a shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Class {
    /// A local of the item: hashed by where it is first used.
    Local,
    /// Any other identifier or lifetime: hashed by its text.
    Name,
    /// A literal: hashed by its kind and text.
    Literal,
    /// A keyword or punctuation: hashed by its kind.
    Fixed,
}

/// One token of a shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Tok {
    pub(crate) class: Class,
    /// A local's number among the item's locals; the hash of a name's or literal's text, or a
    /// fixed token's kind.
    pub(crate) key: u64,
    /// The hash of the token's text, a local's too: what a copy is compared by name with.
    pub(crate) text: u64,
    /// Whether the token binds its local: its name in a pattern.
    pub(crate) binds: bool,
    /// A literal's kind (`INT_NUMBER`, `STRING`, ...) as a number, 0 for any other token: what
    /// [`erased`] reads a literal by.
    pub(crate) lit: u16,
    pub(crate) range: TextRange,
}

/// The tokens of `item`'s shape, in order: [`Tok`] of each but trivia, nested items,
/// attributes and the item's own name and visibility.
pub(crate) fn tokens(item: ast::Node<'_>) -> Vec<Tok> {
    let locals = locals_of(item);
    let mut out = Vec::new_in(Global);
    let mut walk = item.preorder_with_tokens();
    while let Some(event) = walk.next() {
        let WalkEvent::Enter(element) = event else { continue };
        let token = match element {
            NodeOrToken::Node(node) => {
                if node != item && skipped(item, node) {
                    walk.skip_subtree();
                }
                continue;
            }
            NodeOrToken::Token(token) => token,
        };
        let kind = token.kind();
        if kind.is_trivia() {
            continue;
        }
        let text = token.text();
        let text_hash = FixedState.hash_one(text);
        let (class, key, binds) = match kind {
            SyntaxKind::IDENT => {
                let bare = text.strip_prefix("r#").unwrap_or(text);
                match locals.get(bare) {
                    Some(&number) if is_local_use(token) => {
                        (Class::Local, u64::from(number), binds_local(token))
                    }
                    _ => (Class::Name, FixedState.hash_one(bare), false),
                }
            }
            SyntaxKind::LIFETIME_IDENT => (Class::Name, text_hash, false),
            it if it.is_literal() => {
                (Class::Literal, FixedState.hash_one((it as u16, text)), false)
            }
            it => (Class::Fixed, u64::from(it as u16), false),
        };
        let lit = if class == Class::Literal { kind as u16 } else { 0 };
        out.push(Tok { class, key, text: text_hash, binds, lit, range: token.text_range() });
    }
    out
}

/// Whether the walk of `item` leaves `node` out: a nested item, an attribute, or the item's
/// own name and visibility.
fn skipped(item: ast::Node<'_>, node: ast::Node<'_>) -> bool {
    let kind = node.kind();
    ast::Item::can_cast(kind)
        || kind == SyntaxKind::ATTR
        || (matches!(kind, SyntaxKind::NAME | SyntaxKind::VISIBILITY) && node.parent() == Some(item))
}

/// The locals of `item`, numbered in the order they are first bound.
fn locals_of<'t>(item: ast::Node<'t>) -> HashMap<&'t str, u32, Global> {
    let mut locals = HashMap::new_in(Global);
    let mut walk = item.preorder();
    while let Some(event) = walk.next() {
        let WalkEvent::Enter(node) = event else { continue };
        if node != item && skipped(item, node) {
            walk.skip_subtree();
            continue;
        }
        if node.kind() != SyntaxKind::IDENT_PAT {
            continue;
        }
        let Some(name) = node.children().find(|it| it.kind() == SyntaxKind::NAME) else {
            continue;
        };
        let Some(ident) = name.first_token() else { continue };
        let text = ident.text();
        let text = text.strip_prefix("r#").unwrap_or(text);
        if text.starts_with(|it: char| it.is_uppercase()) {
            continue;
        }
        let next = u32::try_from(locals.len()).unwrap_or(u32::MAX);
        locals.entry(text).or_insert(next);
    }
    locals
}

/// Whether the identifier `token`, spelled as a local, is one: its binding, a one-segment path,
/// a field shorthand, or an identifier of a macro call's tokens.
fn is_local_use(token: ast::Token<'_>) -> bool {
    let Some(parent) = token.parent() else { return false };
    match parent.kind() {
        SyntaxKind::NAME => binds_local(token),
        SyntaxKind::TOKEN_TREE => true,
        SyntaxKind::NAME_REF => {
            let Some(up) = parent.parent() else { return false };
            match up.kind() {
                SyntaxKind::RECORD_EXPR_FIELD => {
                    !up.children_with_tokens().any(|it| it.kind() == SyntaxKind::COLON)
                }
                SyntaxKind::PATH_SEGMENT => {
                    let Some(path) = up.parent() else { return false };
                    let qualified = path.children().any(|it| it.kind() == SyntaxKind::PATH);
                    let qualifier = path.parent().is_some_and(|it| it.kind() == SyntaxKind::PATH);
                    let generic = up.children().any(|it| it.kind() != SyntaxKind::NAME_REF);
                    !qualified && !qualifier && !generic
                }
                _ => false,
            }
        }
        _ => false,
    }
}

/// Whether `token` is the name a pattern binds.
fn binds_local(token: ast::Token<'_>) -> bool {
    token.parent().is_some_and(|name| {
        name.kind() == SyntaxKind::NAME
            && name.parent().is_some_and(|it| it.kind() == SyntaxKind::IDENT_PAT)
    })
}

/// The tokens of `toks` within `range`: a contiguous run, as `toks` are in order.
pub(crate) fn within(toks: &[Tok], range: TextRange) -> &[Tok] {
    let start = toks.partition_point(|it| it.range.start() < range.start());
    let end = toks.partition_point(|it| it.range.end() <= range.end());
    toks.get(start..end.max(start)).unwrap_or_default()
}

/// Renumbers locals by their first use in a run: scratch reused across the runs of one item.
#[derive(Default)]
pub(crate) struct Renumber {
    at: Vec<u32, Global>,
    touched: Vec<u32, Global>,
}

impl Renumber {
    /// The number of local `key` in the run so far, from 0 by first use.
    fn number(&mut self, key: u64) -> u32 {
        let index = usize::try_from(key).unwrap_or(usize::MAX);
        if index >= self.at.len() {
            if index > 1 << 20 {
                return u32::MAX;
            }
            self.at.resize(index + 1, u32::MAX);
        }
        let slot = &mut self.at[index];
        if *slot == u32::MAX {
            *slot = u32::try_from(self.touched.len()).unwrap_or(u32::MAX);
            self.touched.push(u32::try_from(index).unwrap_or(u32::MAX));
        }
        *slot
    }

    /// Forgets the run's numbering.
    fn reset(&mut self) {
        for &index in &self.touched {
            self.at[usize::try_from(index).unwrap_or(usize::MAX)] = u32::MAX;
        }
        self.touched.clear();
    }
}

/// The shape hash of a run: each token's class and key, a local's key its number by first use
/// in the run, so renaming locals keeps it and changing a literal, a path or a type changes it.
pub(crate) fn alpha(toks: &[Tok], scratch: &mut Renumber) -> u64 {
    let mut hasher = FixedHasher::default();
    for tok in toks {
        (tok.class as u8).hash(&mut hasher);
        match tok.class {
            Class::Local => scratch.number(tok.key).hash(&mut hasher),
            _ => tok.key.hash(&mut hasher),
        }
    }
    scratch.reset();
    hasher.finish()
}

/// The structure hash of a run: fixed tokens by kind, every identifier and literal by its
/// class alone, so two copies of one code with any names and literals have one.
pub(crate) fn structure(toks: &[Tok]) -> u64 {
    let mut hasher = FixedHasher::with_seed(1);
    for tok in toks {
        match tok.class {
            Class::Fixed => tok.key.hash(&mut hasher),
            Class::Local | Class::Name => 1u64.hash(&mut hasher),
            Class::Literal => 2u64.hash(&mut hasher),
        }
    }
    hasher.finish()
}

/// The shape hash of a run with its literals erased: [`alpha`], but a literal, and `true` and
/// `false`, by the class of literal alone (integer, float, string, character, byte, bool), so
/// two copies that differ only in their values have one. What groups tests of one table.
pub(crate) fn erased(toks: &[Tok], scratch: &mut Renumber) -> u64 {
    let mut hasher = FixedHasher::with_seed(6);
    for tok in toks {
        let bool_kw = tok.class == Class::Fixed
            && (tok.key == u64::from(SyntaxKind::TRUE_KW as u16)
                || tok.key == u64::from(SyntaxKind::FALSE_KW as u16));
        if bool_kw {
            (Class::Literal as u8).hash(&mut hasher);
            u64::from(SyntaxKind::TRUE_KW as u16).hash(&mut hasher);
            continue;
        }
        (tok.class as u8).hash(&mut hasher);
        match tok.class {
            Class::Local => scratch.number(tok.key).hash(&mut hasher),
            Class::Literal => tok.lit.hash(&mut hasher),
            _ => tok.key.hash(&mut hasher),
        }
    }
    scratch.reset();
    hasher.finish()
}

/// The token a shingle reads: a local by its class alone, anything else by its key.
fn shingle_code(tok: &Tok) -> u64 {
    match tok.class {
        Class::Local => u64::MAX,
        _ => tok.key ^ ((tok.class as u64) << 60),
    }
}

/// The shingles of `toks`, sorted and without repeats: the hash of each `width` tokens in a
/// row, locals by their class alone.
pub(crate) fn shingles(toks: &[Tok], width: usize) -> Vec<u64> {
    let mut out = Vec::new_in(Global);
    for window in toks.windows(width) {
        let mut hasher = FixedHasher::with_seed(2);
        for tok in window {
            shingle_code(tok).hash(&mut hasher);
        }
        out.push(hasher.finish());
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// The free locals of a run, each by the range of its first use, in that order: used in the
/// run, bound outside it. What a function made of the run takes as its parameters.
pub(crate) fn free(toks: &[Tok]) -> Vec<TextRange> {
    let mut bound: Vec<u64, Global> = Vec::new_in(Global);
    let mut seen: Vec<u64, Global> = Vec::new_in(Global);
    let mut out = Vec::new_in(Global);
    for tok in toks.iter().filter(|it| it.class == Class::Local) {
        if tok.binds {
            bound.push(tok.key);
            continue;
        }
        if bound.contains(&tok.key) || seen.contains(&tok.key) {
            continue;
        }
        seen.push(tok.key);
        out.push(tok.range);
    }
    out
}

/// A run of code a shape is taken of: a node, or statements in a row of one list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Run {
    /// A block, `if`, `match`, loop, arm or closure.
    Node,
    /// The body block of the function.
    Body,
    /// Statements `first..=last` of the statement list starting at `list`.
    Stmts { list: u32, first: u32, last: u32 },
}

/// A candidate copy of a function: its run, range and shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Piece {
    pub(crate) hash: u64,
    pub(crate) range: TextRange,
    pub(crate) run: Run,
}

/// Whether a node of `kind` is a candidate copy of its own.
fn is_candidate(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::BLOCK_EXPR
            | SyntaxKind::IF_EXPR
            | SyntaxKind::MATCH_EXPR
            | SyntaxKind::MATCH_ARM
            | SyntaxKind::FOR_EXPR
            | SyntaxKind::WHILE_EXPR
            | SyntaxKind::LOOP_EXPR
            | SyntaxKind::CLOSURE_EXPR
    )
}

/// How a piece is hashed: by its shape, or by its structure alone.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum By {
    Shape,
    Structure,
}

/// The candidate copies of `item` of at least `floor` tokens, hashed `by`: its blocks, `if`s,
/// `match`es and their arms, loops and closures, and each shortest row of at least two
/// statements of one list that reaches `floor` (a seed: rows longer than it are chains of
/// seeds, which the index joins). `body` is the item's body block.
pub(crate) fn pieces(
    item: ast::Node<'_>,
    body: Option<ast::Node<'_>>,
    toks: &[Tok],
    floor: u32,
    by: By,
) -> Vec<Piece> {
    let mut out = Vec::new_in(Global);
    let mut scratch = Renumber::default();
    let hash = |run: &[Tok], scratch: &mut Renumber| match by {
        By::Shape => alpha(run, scratch),
        By::Structure => structure(run),
    };
    let weight = |run: &[Tok]| u32::try_from(run.len()).unwrap_or(u32::MAX);
    let mut walk = item.preorder();
    while let Some(event) = walk.next() {
        let WalkEvent::Enter(node) = event else { continue };
        if node != item && skipped(item, node) {
            walk.skip_subtree();
            continue;
        }
        let kind = node.kind();
        if is_candidate(kind) {
            let run = within(toks, node.text_range());
            if weight(run) >= floor {
                let is_body = Some(node) == body;
                out.push(Piece {
                    hash: hash(run, &mut scratch),
                    range: node.text_range(),
                    run: if is_body { Run::Body } else { Run::Node },
                });
            }
        }
        if kind != SyntaxKind::STMT_LIST {
            continue;
        }
        let stmts: Vec<ast::Node<'_>, Global> = statements(node);
        let list = u32::from(node.text_range().start());
        for first in 0..stmts.len() {
            for last in first + 1..stmts.len() {
                if first == 0 && last + 1 == stmts.len() {
                    break;
                }
                let range = stmts[first].text_range().cover(stmts[last].text_range());
                let run = within(toks, range);
                if weight(run) < floor {
                    continue;
                }
                out.push(Piece {
                    hash: hash(run, &mut scratch),
                    range,
                    run: Run::Stmts {
                        list,
                        first: u32::try_from(first).unwrap_or(u32::MAX),
                        last: u32::try_from(last).unwrap_or(u32::MAX),
                    },
                });
                break;
            }
        }
    }
    out
}

/// The statements of a statement list in order, its tail expression last; nested items and
/// attributes are none.
pub(crate) fn statements(list: ast::Node<'_>) -> Vec<ast::Node<'_>, Global> {
    let mut out = Vec::new_in(Global);
    for child in list.children() {
        let kind = child.kind();
        if ast::Item::can_cast(kind) || kind == SyntaxKind::ATTR {
            continue;
        }
        out.push(child);
    }
    out
}
