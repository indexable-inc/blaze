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
struct Open {
    op: Op,
    acc: bool,
    count: u32,
    at: u32,
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
    let list = tokens.tokens().get(..range.end).unwrap_or(&[]);
    let malformed = |at: usize| CfgPredicateError::Malformed { token: u32::try_from(at).unwrap_or(u32::MAX) };
    let mut stack: AVec<'_, Open> = Vec::new_in(alloc);
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
                let acc = op == Op::All;
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
                    Op::Any => top.acc |= v,
                    Op::All => top.acc &= v,
                    Op::Not => {
                        if top.count > 0 {
                            return Err(CfgPredicateError::NotTakesOne { token: top.at });
                        }
                        top.acc = !v;
                    }
                }
                top.count += 1;
                match list.get(at).map(|t| t.kind) {
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
