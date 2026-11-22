//! Rust's cfg predicates: `name`, `name = "value"`, `all(..)`, `any(..)`, `not(..)`, `true`,
//! `false`, held in postfix ([`Expr`]) and lowered to a [`Cond`] over a [`Space`].
//!
//! An atom the space does not hold is constant false, as Rust treats an unset cfg; [`lower`]
//! hands back each such atom's [`VarKey`], because a record that folded one read it (its
//! absence) and must list it (conditions.md, "Absent atoms").

#![blaze::sans_io]

use blaze_own::{AVec, Alloc, planted::Planted};

use crate::bdd::{Cond, PlantedBdd};
use crate::error::{CondError, ParseError};
use crate::plant::CondPlant;
use crate::space::{Atom, NameParts, Space, VarKey};

/// One step of a predicate in postfix.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op<'s> {
    True,
    False,
    Atom(Atom<'s>),
    /// The negation of the one operand before it.
    Not,
    /// The conjunction of the `n` operands before it (`all()` is true).
    All(u32),
    /// The disjunction of the `n` operands before it (`any()` is false).
    Any(u32),
}

/// A cfg predicate in postfix, borrowing its atoms' text.
pub struct Expr<'s, 'a> {
    pub ops: AVec<'a, Op<'s>>,
}

/// The deepest nesting [`parse`] takes (rule 18).
pub const MOST_DEPTH: usize = 256;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fun {
    All,
    Any,
    Not,
}

/// A token of the predicate grammar.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tok<'s> {
    Ident(&'s str),
    Str(&'s str),
    Open,
    Close,
    Comma,
    Eq,
    End,
    Bad,
}

struct Lexer<'s> {
    text: &'s str,
    at: usize,
}

impl<'s> Lexer<'s> {
    fn skip_space(&mut self) {
        let bytes = self.text.as_bytes();
        while self.at < bytes.len() && bytes[self.at].is_ascii_whitespace() {
            self.at += 1;
        }
    }

    /// The next token and where it starts.
    fn next(&mut self) -> (Tok<'s>, usize) {
        self.skip_space();
        let bytes = self.text.as_bytes();
        let start = self.at;
        let Some(&b) = bytes.get(start) else { return (Tok::End, start) };
        let one = |lexer: &mut Lexer<'s>, tok| {
            lexer.at += 1;
            (tok, start)
        };
        match b {
            b'(' => one(self, Tok::Open),
            b')' => one(self, Tok::Close),
            b',' => one(self, Tok::Comma),
            b'=' => one(self, Tok::Eq),
            b'"' => {
                let mut i = start + 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                if i >= bytes.len() {
                    self.at = bytes.len();
                    return (Tok::Bad, start);
                }
                self.at = i + 1;
                (Tok::Str(&self.text[start + 1..i]), start)
            }
            b if b == b'_' || b.is_ascii_alphabetic() || b >= 0x80 => {
                let mut i = start;
                while i < bytes.len() && (bytes[i] == b'_' || bytes[i].is_ascii_alphanumeric() || bytes[i] >= 0x80) {
                    i += 1;
                }
                self.at = i;
                (Tok::Ident(&self.text[start..i]), start)
            }
            _ => (Tok::Bad, start),
        }
    }

    fn peek(&mut self) -> Tok<'s> {
        let at = self.at;
        let (tok, _) = self.next();
        self.at = at;
        tok
    }
}

fn push_op<'s>(ops: &mut AVec<'_, Op<'s>>, op: Op<'s>) -> Result<(), ParseError> {
    ops.try_push(op).map_err(|r| ParseError::Alloc(r.why))
}

/// Parses one predicate at the start of `text` and returns it with the byte offset after it,
/// so `cfg_attr(pred, attrs)` can take its first argument.
pub fn parse_prefix<'s, 'a>(text: &'s str, alloc: Alloc<'a>) -> Result<(Expr<'s, 'a>, usize), ParseError> {
    let mut ops = AVec::new_in(alloc);
    let mut open: AVec<'a, (Fun, u32, usize)> = AVec::new_in(alloc);
    let mut lexer = Lexer { text, at: 0 };
    loop {
        // A predicate.
        let (tok, at) = lexer.next();
        match tok {
            Tok::Ident(name) => match lexer.peek() {
                Tok::Open => {
                    lexer.next();
                    let fun = match name {
                        "all" => Fun::All,
                        "any" => Fun::Any,
                        "not" => Fun::Not,
                        _ => return Err(ParseError::UnknownFunction { at }),
                    };
                    if open.len() >= MOST_DEPTH {
                        return Err(ParseError::TooDeep { at });
                    }
                    open.try_push((fun, 0, at)).map_err(|r| ParseError::Alloc(r.why))?;
                    // `all()` and `any()`: closed at once.
                    if lexer.peek() == Tok::Close {
                        if fun == Fun::Not {
                            return Err(ParseError::NotArity { at });
                        }
                        lexer.next();
                        open.pop();
                        push_op(&mut ops, if fun == Fun::All { Op::All(0) } else { Op::Any(0) })?;
                    } else {
                        continue;
                    }
                }
                Tok::Eq => {
                    lexer.next();
                    let (value, vat) = lexer.next();
                    let Tok::Str(value) = value else {
                        return Err(if value == Tok::End { ParseError::End { at: vat } } else { ParseError::Unexpected { at: vat } });
                    };
                    push_op(&mut ops, Op::Atom(Atom::pair(name, value)))?;
                }
                _ => push_op(
                    &mut ops,
                    match name {
                        "true" => Op::True,
                        "false" => Op::False,
                        _ => Op::Atom(Atom::flag(name)),
                    },
                )?,
            },
            Tok::End => return Err(ParseError::End { at }),
            _ => return Err(ParseError::Unexpected { at }),
        }
        // After a complete predicate: close what it completes.
        loop {
            let Some(top) = open.last_mut() else { return Ok((Expr { ops }, lexer.at)) };
            top.1 += 1;
            let (fun, count, fat) = *top;
            let (tok, at) = lexer.next();
            let close = match tok {
                Tok::Comma => {
                    if lexer.peek() == Tok::Close && fun != Fun::Not {
                        lexer.next();
                        true
                    } else {
                        false
                    }
                }
                Tok::Close => true,
                Tok::End => return Err(ParseError::End { at }),
                _ => return Err(ParseError::Unexpected { at }),
            };
            if !close {
                if fun == Fun::Not {
                    return Err(ParseError::NotArity { at: fat });
                }
                break;
            }
            open.pop();
            push_op(
                &mut ops,
                match fun {
                    Fun::All => Op::All(count),
                    Fun::Any => Op::Any(count),
                    Fun::Not if count == 1 => Op::Not,
                    Fun::Not => return Err(ParseError::NotArity { at: fat }),
                },
            )?;
        }
    }
}

impl<'s> Expr<'s, '_> {
    /// Whether the predicate holds where `has` says which atoms are set: the one evaluation of
    /// a predicate over a plain list of atoms (a target's `--print cfg`, a config's
    /// `target.'cfg(..)'` table matched against it), where [`lower`] is the one over a space.
    pub fn holds(&self, has: impl Fn(Atom<'s>) -> bool, alloc: Alloc<'_>) -> bool {
        let mut stack: AVec<'_, bool> = blaze_own::Vec::new_in(alloc);
        for op in self.ops.iter() {
            let value = match *op {
                Op::True => true,
                Op::False => false,
                Op::Atom(atom) => has(atom),
                Op::Not => !stack.pop().unwrap_or(false),
                Op::All(n) => {
                    let mut all = true;
                    for _ in 0..n {
                        all &= stack.pop().unwrap_or(true);
                    }
                    all
                }
                Op::Any(n) => {
                    let mut any = false;
                    for _ in 0..n {
                        any |= stack.pop().unwrap_or(false);
                    }
                    any
                }
            };
            stack.push(value);
        }
        stack.pop().unwrap_or(false)
    }

    /// The one atom this predicate is, when it is a lone `name` or `name = "value"` (a rustc
    /// `--cfg` spec); `None` for `true`, `false` or any `all`/`any`/`not`.
    pub fn lone_atom(&self) -> Option<Atom<'s>> {
        match self.ops.as_slice() {
            [Op::Atom(atom)] => Some(*atom),
            _ => None,
        }
    }
}

/// Parses a whole predicate: nothing but space may follow it.
pub fn parse<'s, 'a>(text: &'s str, alloc: Alloc<'a>) -> Result<Expr<'s, 'a>, ParseError> {
    let (expr, end) = parse_prefix(text, alloc)?;
    let mut lexer = Lexer { text, at: end };
    match lexer.next() {
        (Tok::End, _) => Ok(expr),
        (_, at) => Err(ParseError::Unexpected { at }),
    }
}

/// `expr`'s condition over `space`, in `bdd`, for a crate of `package` (whose `feature = "x"`
/// atoms are its features). Each atom the space does not hold is false, and its key is pushed
/// to `absent`.
pub fn lower(
    expr: &Expr<'_, '_>,
    space: &Space<'_>,
    package: &str,
    bdd: &mut PlantedBdd<'_, impl Planted<CondPlant>>,
    absent: &mut AVec<'_, VarKey>,
) -> Result<Cond, CondError> {
    let mut stack: AVec<'_, Cond> = AVec::new_in(bdd.alloc());
    for (at, op) in expr.ops.iter().enumerate() {
        let push = |stack: &mut AVec<'_, Cond>, c| stack.try_push(c).map_err(|r| CondError::Alloc(r.why));
        match *op {
            Op::True => push(&mut stack, Cond::TRUE)?,
            Op::False => push(&mut stack, Cond::FALSE)?,
            Op::Atom(atom) => match space.atom(package, atom) {
                Some(var) => {
                    let c = bdd.var(var)?;
                    push(&mut stack, c)?;
                }
                None => {
                    let key = absent_key(package, atom);
                    absent.try_push(key).map_err(|r| CondError::Alloc(r.why))?;
                    push(&mut stack, Cond::FALSE)?;
                }
            },
            Op::Not => {
                let Some(c) = stack.pop() else { return Err(CondError::MalformedExpr { at }) };
                push(&mut stack, c.not())?;
            }
            Op::All(n) | Op::Any(n) => {
                let all = matches!(op, Op::All(_));
                let n = usize::try_from(n).map_err(|_| CondError::MalformedExpr { at })?;
                if stack.len() < n {
                    return Err(CondError::MalformedExpr { at });
                }
                let mut acc = if all { Cond::TRUE } else { Cond::FALSE };
                for _ in 0..n {
                    let Some(c) = stack.pop() else { return Err(CondError::MalformedExpr { at }) };
                    acc = if all { bdd.and(acc, c)? } else { bdd.or(acc, c)? };
                }
                push(&mut stack, acc)?;
            }
        }
    }
    match (stack.pop(), stack.is_empty()) {
        (Some(c), true) => Ok(c),
        _ => Err(CondError::MalformedExpr { at: expr.ops.len() }),
    }
}

/// The key an absent atom would have.
fn absent_key(package: &str, atom: Atom<'_>) -> VarKey {
    match (atom.name, atom.value) {
        ("feature", Some(feature)) => NameParts::feature(package, feature).key(),
        _ => NameParts::atom(atom).key(),
    }
}
