//! A cfg predicate's outcome, read from its tokens (`unix`, `feature = "x"`, `any(..)`,
//! `all(..)`, `not(..)`), each atom asked of the caller's cfg set: what an [`crate::ExpandCx`]
//! answers `cfg!` and `#[cfg]` with. Sans-IO, no recursion (an explicit stack of the groups
//! open), every atom asked once in source order, the way the Reference's "Conditional
//! compilation" chapter evaluates one.

use core::fmt;
use core::ops::Range;

use blaze_own::{AVec, Alloc, Vec};

use crate::literal::{Buf, string_value};
use crate::tokens::{Delim, TokenKind, TokenStream};

/// Why a predicate has no outcome.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CfgPredicateError<E> {
    /// The tokens are no predicate: the token at this index is where it stops.
    Malformed { token: u32 },
    /// `not(..)` holds no predicate, or more than one, at this token.
    NotTakesOne { token: u32 },
    /// The cfg set could not answer an atom.
    Atom(E),
}

impl<E: fmt::Debug> fmt::Display for CfgPredicateError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CfgPredicateError::Malformed { token } => write!(f, "no cfg predicate: it stops at token {token}"),
            CfgPredicateError::NotTakesOne { token } => write!(f, "`not(..)` takes one predicate (token {token})"),
            CfgPredicateError::Atom(e) => write!(f, "a cfg atom has no answer: {e:?}"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Any,
    All,
    Not,
}

/// A group open: its operator, the outcome so far, and how many predicates it holds.
#[derive(Clone, Copy)]
struct Open<V> {
    op: Op,
    acc: V,
    count: u32,
    at: u32,
}

/// A predicate's outcome as it is folded: two-valued (`bool`: the build's answer, every atom
/// known) or Kleene's three values ([`Tri`]: what is known before the cfg set is whole). ONE
/// parser reads both ([`predicate_in`], rule 30), so the two never read a predicate apart.
trait Truth: Copy {
    fn of(value: bool) -> Self;
    fn or(self, other: Self) -> Self;
    fn and(self, other: Self) -> Self;
    fn not(self) -> Self;
}

impl Truth for bool {
    fn of(value: bool) -> bool {
        value
    }
    fn or(self, other: bool) -> bool {
        self | other
    }
    fn and(self, other: bool) -> bool {
        self & other
    }
    fn not(self) -> bool {
        !self
    }
}

/// A predicate's outcome when some atoms are not known yet (Kleene's strong three-valued
/// logic): `Yes` and `No` are what every completion of the unknown atoms gives, so a `No` is
/// never wrong; `Unknown` is any outcome some completion could move ([`cfg_predicate_tri`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tri {
    No,
    Yes,
    Unknown,
}

impl Tri {
    /// A known value.
    pub fn of(value: bool) -> Tri {
        if value { Tri::Yes } else { Tri::No }
    }

    /// The two-valued outcome, when it is known.
    pub fn known(self) -> Option<bool> {
        match self {
            Tri::No => Some(false),
            Tri::Yes => Some(true),
            Tri::Unknown => None,
        }
    }

    /// Both hold (Kleene's `and`): `No` when either is, `Yes` when both are.
    pub fn and(self, other: Tri) -> Tri {
        match (self, other) {
            (Tri::No, _) | (_, Tri::No) => Tri::No,
            (Tri::Yes, Tri::Yes) => Tri::Yes,
            _ => Tri::Unknown,
        }
    }
}

impl Truth for Tri {
    fn of(value: bool) -> Tri {
        Tri::of(value)
    }
    fn or(self, other: Tri) -> Tri {
        match (self, other) {
            (Tri::Yes, _) | (_, Tri::Yes) => Tri::Yes,
            (Tri::No, Tri::No) => Tri::No,
            _ => Tri::Unknown,
        }
    }
    fn and(self, other: Tri) -> Tri {
        Tri::and(self, other)
    }
    fn not(self) -> Tri {
        match self {
            Tri::No => Tri::Yes,
            Tri::Yes => Tri::No,
            Tri::Unknown => Tri::Unknown,
        }
    }
}

/// The outcome of the predicate `tokens`, asking `atom(name, value)` for each atom (`value` is
/// a `name = "value"` atom's string, unescaped). Trailing commas are taken; an empty `any()` is
/// false and an empty `all()` true.
pub fn cfg_predicate<E>(
    tokens: &TokenStream<'_>,
    alloc: Alloc<'_>,
    atom: &mut dyn FnMut(&str, Option<&str>) -> Result<bool, E>,
) -> Result<bool, CfgPredicateError<E>> {
    cfg_predicate_at(tokens, 0..tokens.tokens().len(), alloc, atom)
}

/// [`cfg_predicate`] of the tokens `range` of `tokens` (a `cfg_attr`'s first argument); token
/// indices in its errors are `tokens`'.
pub fn cfg_predicate_at<E>(
    tokens: &TokenStream<'_>,
    range: Range<usize>,
    alloc: Alloc<'_>,
    atom: &mut dyn FnMut(&str, Option<&str>) -> Result<bool, E>,
) -> Result<bool, CfgPredicateError<E>> {
    predicate_in::<bool, E>(tokens, range, alloc, atom)
}

/// [`cfg_predicate_at`] in Kleene's three values: `atom` answers [`Tri::Unknown`] for an atom not
/// known yet (plan time knows a unit's `test` before its whole cfg set), and the outcome is
/// `No` only when every answer the unknown atoms could have gives `false`. The same parser as the
/// two-valued outcome ([`predicate_in`]): where every atom is known, the two agree.
pub fn cfg_predicate_tri(
    tokens: &TokenStream<'_>,
    range: Range<usize>,
    alloc: Alloc<'_>,
    atom: &mut dyn FnMut(&str, Option<&str>) -> Tri,
) -> Result<Tri, CfgPredicateError<core::convert::Infallible>> {
    predicate_in::<Tri, core::convert::Infallible>(tokens, range, alloc, &mut |name, value| Ok(atom(name, value)))
}

/// THE ONE READING of a cfg predicate (the module doc), folded in `V`.
fn predicate_in<V: Truth, E>(
    tokens: &TokenStream<'_>,
    range: Range<usize>,
    alloc: Alloc<'_>,
    atom: &mut dyn FnMut(&str, Option<&str>) -> Result<V, E>,
) -> Result<V, CfgPredicateError<E>> {
    let list = tokens.tokens().get(..range.end).unwrap_or(&[]);
    let malformed = |at: usize| CfgPredicateError::Malformed { token: u32::try_from(at).unwrap_or(u32::MAX) };
    let mut stack: AVec<'_, Open<V>> = Vec::new_in(alloc);
    let mut value_buf = Buf::new_in(alloc);
    let mut at = range.start;
    loop {
        // A predicate starts here: a group opens, or an atom gives a value.
        let token = list.get(at).ok_or(malformed(at))?;
        let TokenKind::Ident { .. } = token.kind else { return Err(malformed(at)) };
        let name = tokens.text_of(token);
        let next = list.get(at + 1).map(|t| t.kind);
        let op = match name {
            "any" => Some(Op::Any),
            "all" => Some(Op::All),
            "not" => Some(Op::Not),
            _ => None,
        };
        let mut value = match (op, next) {
            (Some(op), Some(TokenKind::Open(Delim::Paren))) => {
                let acc = V::of(op == Op::All);
                stack.push(Open { op, acc, count: 0, at: u32::try_from(at).unwrap_or(u32::MAX) });
                at += 2;
                // An empty group closes at once.
                if matches!(list.get(at).map(|t| t.kind), Some(TokenKind::Close(Delim::Paren))) {
                    None
                } else {
                    continue;
                }
            }
            (_, Some(TokenKind::Punct { char: '=', .. })) => {
                let lit = list.get(at + 2).ok_or(malformed(at + 2))?;
                let TokenKind::Literal { kind, suffix_len, .. } = lit.kind else { return Err(malformed(at + 2)) };
                value_buf.0.clear();
                string_value(kind, tokens.text_of(lit), suffix_len, &mut value_buf).map_err(|_| malformed(at + 2))?;
                at += 3;
                Some(atom(name, Some(value_buf.as_str())).map_err(CfgPredicateError::Atom)?)
            }
            _ => {
                at += 1;
                Some(atom(name, None).map_err(CfgPredicateError::Atom)?)
            }
        };
        // Fold values into the groups open, closing each group its `)` ends.
        loop {
            let Some(top) = stack.last_mut() else {
                // The whole predicate: nothing may follow it.
                return match (value, list.get(at)) {
                    (Some(v), None) => Ok(v),
                    _ => Err(malformed(at)),
                };
            };
            if let Some(v) = value.take() {
                match top.op {
                    Op::Any => top.acc = top.acc.or(v),
                    Op::All => top.acc = top.acc.and(v),
                    Op::Not => {
                        if top.count > 0 {
                            return Err(CfgPredicateError::NotTakesOne { token: top.at });
                        }
                        top.acc = v.not();
                    }
                }
                top.count += 1;
                match list.get(at).map(|t| t.kind) {
                    // astlog-ignore: expand-hand-extent (the separator after a predicate this fold read whole; the cfg grammar holds no generics)
                    Some(TokenKind::Punct { char: ',', .. }) => {
                        at += 1;
                        if !matches!(list.get(at).map(|t| t.kind), Some(TokenKind::Close(Delim::Paren))) {
                            break;
                        }
                    }
                    Some(TokenKind::Close(Delim::Paren)) => {}
                    _ => return Err(malformed(at)),
                }
            }
            // At the group's `)`.
            let Some(TokenKind::Close(Delim::Paren)) = list.get(at).map(|t| t.kind) else { return Err(malformed(at)) };
            let done = stack.pop().ok_or(malformed(at))?;
            if done.op == Op::Not && done.count != 1 {
                return Err(CfgPredicateError::NotTakesOne { token: done.at });
            }
            at += 1;
            value = Some(done.acc);
        }
    }
}

/// Why a `#[cfg_attr(..)]` leaves no answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CfgAttrError<E> {
    /// The tokens are not `#[cfg_attr(..)]`.
    NotCfgAttr,
    /// A `cfg_attr(..)` with no predicate before its first `,`, at this token.
    NoPredicate { token: u32 },
    /// Its predicate has no outcome.
    Predicate(CfgPredicateError<E>),
}

/// The attributes `#[cfg_attr(pred, a, b(..), ..)]` (`attr`'s tokens, from the `#`) leaves under
/// the cfg set `atom` answers, in order, each one's meta (`a`, `b(..)`) as a token range of
/// `attr`: none when `pred` is false. A `cfg_attr(..)` among them is applied in its place, as
/// the Reference's "Conditional compilation" chapter orders it (a stack of the metas left, no
/// recursion). An empty meta between commas is skipped, as a trailing comma is.
pub fn cfg_attr_metas<'a, E>(
    attr: &TokenStream<'_>,
    alloc: Alloc<'a>,
    atom: &mut dyn FnMut(&str, Option<&str>) -> Result<bool, E>,
) -> Result<AVec<'a, Range<u32>>, CfgAttrError<E>> {
    let list = attr.tokens();
    let is_hash = matches!(list.first().map(|t| t.kind), Some(TokenKind::Punct { char: '#', .. }));
    let is_bracket = matches!(list.get(1).map(|t| t.kind), Some(TokenKind::Open(Delim::Bracket)));
    if !(is_hash && is_bracket && attr.close_of(1) == Some(list.len().wrapping_sub(1))) {
        return Err(CfgAttrError::NotCfgAttr);
    }
    let index = |at: usize| u32::try_from(at).unwrap_or(u32::MAX);
    // `name(..)` at `m`: the range inside its parentheses when `name` is `cfg_attr`.
    let cfg_attr_inside = |m: &Range<usize>| -> Option<Range<usize>> {
        let is_name = matches!(list.get(m.start).map(|t| t.kind), Some(TokenKind::Ident { .. })) && attr.text_at(m.start) == "cfg_attr";
        let is_paren = matches!(list.get(m.start + 1).map(|t| t.kind), Some(TokenKind::Open(Delim::Paren)));
        (is_name && is_paren && attr.close_of(m.start + 1) == Some(m.end - 1)).then(|| m.start + 2..m.end - 1)
    };
    let whole = 2..list.len() - 1;
    if cfg_attr_inside(&whole).is_none() {
        return Err(CfgAttrError::NotCfgAttr);
    }
    let mut out = Vec::new_in(alloc);
    // The metas left to read, the next one last.
    let mut left: AVec<'_, Range<usize>> = Vec::new_in(alloc);
    let mut pieces: AVec<'_, Range<usize>> = Vec::new_in(alloc);
    left.push(whole);
    while let Some(meta) = left.pop() {
        let Some(inside) = cfg_attr_inside(&meta) else {
            out.push(index(meta.start)..index(meta.end));
            continue;
        };
        // Its arguments, split at the commas outside any group.
        pieces.clear();
        let (mut start, mut depth) = (inside.start, 0u32);
        for at in inside.clone() {
            match list[at].kind {
                TokenKind::Open(_) => depth += 1,
                TokenKind::Close(_) => depth = depth.saturating_sub(1),
                // astlog-ignore: expand-hand-extent (LEFT: cfg_attr's metas by depth-0 commas; to move onto the MetaItem tree's CFG_ATTR_META with a CfgAttrError variant and blaze-ladder foreign_locals.rs in one batch)
                TokenKind::Punct { char: ',', .. } if depth == 0 => {
                    pieces.push(start..at);
                    start = at + 1;
                }
                _ => {}
            }
        }
        pieces.push(start..inside.end);
        let predicate = pieces[0].clone();
        if predicate.is_empty() {
            return Err(CfgAttrError::NoPredicate { token: index(meta.start) });
        }
        if cfg_predicate_at(attr, predicate, alloc, atom).map_err(CfgAttrError::Predicate)? {
            left.extend(pieces[1..].iter().rev().filter(|p| !p.is_empty()).cloned());
        }
    }
    Ok(out)
}
