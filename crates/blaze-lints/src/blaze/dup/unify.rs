//! Anti-unification of two copies: their least general generalisation (Plotkin 1970), as
//! CloneDigger (Bulychev and Minea 2008) and IntelliJ's "extract from duplicates" make it. The
//! two trees are walked together; where the kinds and the children agree the walk goes on,
//! and where they differ the smallest subtree pair that differs is a hole: what a function
//! made of the two takes as a parameter. A local renamed the same way everywhere is no hole
//! (its two names are a bijection, kept as the walk goes); two holes holding the same pair of
//! texts are one parameter. Pure over trees, like [`super::shape`]: the tests parse text.
//!
//! What a pair generalises to decides what it is: holes of literals, paths, fields and
//! expressions of one type each are a function with parameters (`extractable_duplicate`);
//! literals alone between tests are a table (`tests_differ_by_literal`); types alone are one
//! generic function (`impls_differ_by_type`).

use std::alloc::Global;

use blaze_hash::HashMap;
use syntax::{
    NodeOrToken, SyntaxKind, TextRange,
    view::{self as ast, AstNode as _},
};

use super::shape::{Class, Tok};

/// The most parameters a generalisation has: past it a function made of the two copies takes
/// more arguments than a reader holds at once (four is where clippy's `too_many_arguments` is
/// not yet, and where CloneDigger's reports stop being refactorings people make), and the pair
/// is two functions that look alike, which `near_duplicate` says.
pub(crate) const HOLES_MOST: usize = 4;
/// The largest share of the first copy's tokens, in percent, a hole of statements may hold:
/// past it the copies differ in what they do, not in a value they are given, and a swapped
/// order of statements is not a parameter.
pub(crate) const STATEMENTS_MOST_PERCENT: usize = 20;

/// What a hole holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum HoleClass {
    /// A literal: `64` and `256`.
    Literal,
    /// A path in an expression: a callee, a constant, a static, a variant.
    Path,
    /// A field's name: `.head` and `.tail`.
    Field,
    /// A method's name: `.read_head()` and `.read_tail()`.
    Method,
    /// A type.
    Type,
    /// Any other expression, or an operator.
    Expr,
    /// Statements: a different count or kind of them.
    Statements,
}

/// One parameter of a generalisation: where it is in each copy, the first place it is used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Hole {
    pub(crate) a: TextRange,
    pub(crate) b: TextRange,
    pub(crate) class: HoleClass,
    /// The places it is used, at least one.
    pub(crate) uses: u32,
}

/// The generalisation of two copies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Generalised {
    /// The parameters, in the order the first copy uses them.
    pub(crate) holes: Vec<Hole, Global>,
    /// The tokens of the first copy outside every hole.
    pub(crate) common_tokens: u32,
}

/// The generalisation of `a` and `b`, two items (functions, or any two nodes), whose shape
/// tokens are `toks_a` and `toks_b` ([`super::shape::tokens`], which say which identifiers are
/// locals); `None` past [`HOLES_MOST`] parameters, or with a hole of statements past
/// [`STATEMENTS_MOST_PERCENT`] of `a`. The items' own names, visibility and attributes are
/// not compared.
pub(crate) fn unify(a: ast::Node<'_>, b: ast::Node<'_>, toks_a: &[Tok], toks_b: &[Tok]) -> Option<Generalised> {
    let mut walk = Walk {
        top: a,
        toks: (toks_a, toks_b),
        forward: HashMap::new_in(Global),
        backward: HashMap::new_in(Global),
        raw: Vec::new_in(Global),
    };
    walk.node(a, b);
    let total = toks_a.len().max(1);
    let mut holes: Vec<Hole, Global> = Vec::new_in(Global);
    let mut texts: Vec<(&str, &str), Global> = Vec::new_in(Global);
    let mut in_holes: Vec<TextRange, Global> = Vec::new_in(Global);
    // Outer holes first, so a hole inside one is part of it.
    walk.raw.sort_by_key(|it| (it.a.start(), std::cmp::Reverse(it.a.end())));
    for raw in walk.raw.iter() {
        if raw.class == HoleClass::Statements {
            let held = super::shape::within(toks_a, raw.a).len().max(super::shape::within(toks_b, raw.b).len());
            if held * 100 > total * STATEMENTS_MOST_PERCENT {
                return None;
            }
        }
        if in_holes.iter().any(|it| it.contains_range(raw.a)) {
            continue;
        }
        in_holes.push(raw.a);
        let pair = (text_of(a, raw.a), text_of(b, raw.b));
        match texts.iter().position(|it| *it == pair) {
            Some(at) => holes[at].uses += 1,
            None => {
                texts.push(pair);
                holes.push(Hole { a: raw.a, b: raw.b, class: raw.class, uses: 1 });
            }
        }
        if holes.len() > HOLES_MOST {
            return None;
        }
    }
    let inside: usize = in_holes.iter().map(|it| super::shape::within(toks_a, *it).len()).sum();
    let common_tokens = u32::try_from(toks_a.len().saturating_sub(inside)).unwrap_or(u32::MAX);
    Some(Generalised { holes, common_tokens })
}

/// The text of `range` of `node`'s tree.
fn text_of<'t>(node: ast::Node<'t>, range: TextRange) -> &'t str {
    super::index::text_at(node, range)
}

/// A hole as the walk finds it, before holes of one pair of texts are joined.
struct Raw {
    a: TextRange,
    b: TextRange,
    class: HoleClass,
}

struct Walk<'t, 'k> {
    /// The first item: its own name and visibility are not compared.
    top: ast::Node<'t>,
    toks: (&'k [Tok], &'k [Tok]),
    /// The locals of `a` renamed to those of `b`, by their numbers, and back.
    forward: HashMap<u64, u64, Global>,
    backward: HashMap<u64, u64, Global>,
    raw: Vec<Raw, Global>,
}

/// The children of `node` a generalisation compares: no trivia, and for the top item no name,
/// visibility or attribute.
fn compared<'t>(node: ast::Node<'t>, top: bool) -> Vec<ast::Element<'t>, Global> {
    let mut out = Vec::new_in(Global);
    for child in node.children_with_tokens() {
        let kind = child.kind();
        if kind.is_trivia() || kind == SyntaxKind::ATTR {
            continue;
        }
        if top && matches!(kind, SyntaxKind::NAME | SyntaxKind::VISIBILITY) {
            continue;
        }
        out.push(child);
    }
    out
}

impl<'t> Walk<'t, '_> {
    fn node(&mut self, a: ast::Node<'t>, b: ast::Node<'t>) {
        let top = a == self.top;
        if a.kind() != b.kind() {
            self.hole(a, b);
            return;
        }
        let (xs, ys) = (compared(a, top), compared(b, top));
        if xs.len() != ys.len() {
            self.hole(a, b);
            return;
        }
        for (x, y) in xs.into_iter().zip(ys) {
            match (x, y) {
                (NodeOrToken::Node(x), NodeOrToken::Node(y)) => self.node(x, y),
                (NodeOrToken::Token(x), NodeOrToken::Token(y)) => {
                    if !self.same_token(x, y) {
                        self.hole(a, b);
                        return;
                    }
                }
                _ => {
                    self.hole(a, b);
                    return;
                }
            }
        }
    }

    /// Whether two tokens are one: the same kind and text, or two locals renamed one to the
    /// other everywhere so far.
    fn same_token(&mut self, x: ast::Token<'t>, y: ast::Token<'t>) -> bool {
        if x.kind() != y.kind() {
            return false;
        }
        let local = |toks: &[Tok], range: TextRange| {
            let at = toks.partition_point(|it| it.range.start() < range.start());
            toks.get(at).filter(|it| it.range == range && it.class == Class::Local).map(|it| it.key)
        };
        match (local(self.toks.0, x.text_range()), local(self.toks.1, y.text_range())) {
            (Some(p), Some(q)) => {
                let there = *self.forward.entry(p).or_insert(q);
                let back = *self.backward.entry(q).or_insert(p);
                there == q && back == p
            }
            (None, None) => x.text() == y.text(),
            _ => false,
        }
    }

    /// Records the pair `a`, `b` as a hole, lifted to what a parameter would stand for: a
    /// name to its whole path, a literal's token to the literal.
    fn hole(&mut self, a: ast::Node<'t>, b: ast::Node<'t>) {
        let (a, b) = lift(a, b);
        let class = classify(a, b);
        self.raw.push(Raw { a: a.text_range(), b: b.text_range(), class });
    }
}

/// `a` and `b` moved up together through the nodes of a path, to the outermost path; a
/// field's or method's name stays its name.
fn lift<'t>(mut a: ast::Node<'t>, mut b: ast::Node<'t>) -> (ast::Node<'t>, ast::Node<'t>) {
    let in_path = |kind: SyntaxKind| {
        matches!(kind, SyntaxKind::NAME_REF | SyntaxKind::PATH_SEGMENT | SyntaxKind::PATH)
    };
    let member = |node: ast::Node<'_>| {
        node.kind() == SyntaxKind::NAME_REF
            && node.parent().is_some_and(|it| {
                matches!(it.kind(), SyntaxKind::FIELD_EXPR | SyntaxKind::METHOD_CALL_EXPR)
            })
    };
    if member(a) && member(b) {
        return (a, b);
    }
    while in_path(a.kind()) && in_path(b.kind()) {
        let (Some(x), Some(y)) = (a.parent(), b.parent()) else { break };
        if !(in_path(x.kind()) && in_path(y.kind())) {
            break;
        }
        a = x;
        b = y;
    }
    (a, b)
}

/// What a hole holds, by its two sides.
fn classify(a: ast::Node<'_>, b: ast::Node<'_>) -> HoleClass {
    let both = |kind: SyntaxKind| a.kind() == kind && b.kind() == kind;
    let parents = |kind: SyntaxKind| {
        a.parent().is_some_and(|it| it.kind() == kind) && b.parent().is_some_and(|it| it.kind() == kind)
    };
    if both(SyntaxKind::LITERAL) {
        return HoleClass::Literal;
    }
    if both(SyntaxKind::NAME_REF) && parents(SyntaxKind::FIELD_EXPR) {
        return HoleClass::Field;
    }
    if both(SyntaxKind::NAME_REF) && parents(SyntaxKind::METHOD_CALL_EXPR) {
        return HoleClass::Method;
    }
    if both(SyntaxKind::PATH) {
        if parents(SyntaxKind::PATH_TYPE) {
            return HoleClass::Type;
        }
        if parents(SyntaxKind::PATH_EXPR) {
            return HoleClass::Path;
        }
    }
    if ast::Type::can_cast(a.kind()) && ast::Type::can_cast(b.kind()) {
        return HoleClass::Type;
    }
    let statement = |node: ast::Node<'_>| {
        matches!(node.kind(), SyntaxKind::STMT_LIST | SyntaxKind::EXPR_STMT | SyntaxKind::LET_STMT)
            || node.parent().is_some_and(|it| it.kind() == SyntaxKind::STMT_LIST)
    };
    if statement(a) || statement(b) {
        return HoleClass::Statements;
    }
    HoleClass::Expr
}
