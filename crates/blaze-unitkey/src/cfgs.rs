//! `#[cfg]` predicates as bytes in a file image, and their value under a unit's options.
//!
//! An expression is written in preorder: [`ALL`] or [`ANY`] and a `u16` count, then that many
//! expressions; [`NOT`] then one; [`FLAG`], a `u16` length and the name; [`KEY_VALUE`], the key and
//! the value each as a `u16` length and bytes. The image is keyed by the file's bytes, so the
//! encoding is a function of them; its value is the unit's, decided at keying.

#![blaze::sans_io]

use core::alloc::Allocator;

use cfg::{CfgAtom, CfgExpr, CfgOptions};
use intern::Symbol;

pub const ALL: u8 = 1;
pub const ANY: u8 = 2;
pub const NOT: u8 = 3;
pub const FLAG: u8 = 4;
pub const KEY_VALUE: u8 = 5;

/// Why a predicate is not written: rustc refuses each of these.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CfgRefused {
    /// A predicate that does not parse (`cfg(a(b))`, a value that is not a string).
    Invalid,
    /// More than `u16::MAX` operands or a name longer than that.
    TooLarge,
}

impl CfgRefused {
    pub fn rule(self) -> &'static str {
        match self {
            CfgRefused::Invalid => "a `cfg` predicate that does not parse",
            CfgRefused::TooLarge => "a `cfg` predicate past 2^16 operands or bytes",
        }
    }
}

/// Appends `expr` to `out`.
pub fn encode<A: Allocator>(expr: &CfgExpr, out: &mut Vec<u8, A>) -> Result<(), CfgRefused> {
    match expr {
        CfgExpr::Invalid => Err(CfgRefused::Invalid),
        CfgExpr::Atom(CfgAtom::Flag(name)) => {
            out.push(FLAG);
            bytes(name.as_str().as_bytes(), out)
        }
        CfgExpr::Atom(CfgAtom::KeyValue { key, value }) => {
            out.push(KEY_VALUE);
            bytes(key.as_str().as_bytes(), out)?;
            bytes(value.as_str().as_bytes(), out)
        }
        CfgExpr::All(exprs) => list(ALL, exprs, out),
        CfgExpr::Any(exprs) => list(ANY, exprs, out),
        CfgExpr::Not(inner) => {
            out.push(NOT);
            encode(inner, out)
        }
    }
}

fn list<A: Allocator>(tag: u8, exprs: &[CfgExpr], out: &mut Vec<u8, A>) -> Result<(), CfgRefused> {
    out.push(tag);
    let count = u16::try_from(exprs.len()).map_err(|_| CfgRefused::TooLarge)?;
    out.extend_from_slice(&count.to_le_bytes());
    exprs.iter().try_for_each(|expr| encode(expr, out))
}

fn bytes<A: Allocator>(text: &[u8], out: &mut Vec<u8, A>) -> Result<(), CfgRefused> {
    let len = u16::try_from(text.len()).map_err(|_| CfgRefused::TooLarge)?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(text);
    Ok(())
}

/// Appends `ANY(NOT cond, inner)`'s head: what `cfg_attr(cond, cfg(inner))` means; the caller
/// writes `cond` and then `inner` after it.
pub fn implies_head<A: Allocator>(out: &mut Vec<u8, A>) {
    out.push(ANY);
    out.extend_from_slice(&2u16.to_le_bytes());
    out.push(NOT);
}

/// Appends an `ALL` of `count` expressions' head.
pub fn all_head<A: Allocator>(count: u16, out: &mut Vec<u8, A>) {
    out.push(ALL);
    out.extend_from_slice(&count.to_le_bytes());
}

/// A predicate's encoding that does not decode: the image is not one this crate wrote.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Undecodable;

/// The value of the encoded predicate `bytes` under `options`; `true` for none (empty).
///
/// Cost: one walk of the encoding, one interning per atom.
pub fn holds(bytes: &[u8], options: &CfgOptions) -> Result<bool, Undecodable> {
    if bytes.is_empty() {
        return Ok(true);
    }
    let mut at = 0;
    let value = eval(bytes, &mut at, options)?;
    match at == bytes.len() {
        true => Ok(value),
        false => Err(Undecodable),
    }
}

fn eval(bytes: &[u8], at: &mut usize, options: &CfgOptions) -> Result<bool, Undecodable> {
    let tag = *bytes.get(*at).ok_or(Undecodable)?;
    *at += 1;
    match tag {
        ALL | ANY => {
            let count = u16_at(bytes, at)?;
            // Every operand is read, whatever the value so far: the walk must end where the
            // expression does.
            let mut value = tag == ALL;
            for _ in 0..count {
                let operand = eval(bytes, at, options)?;
                value = match tag == ALL {
                    true => value && operand,
                    false => value || operand,
                };
            }
            Ok(value)
        }
        NOT => Ok(!eval(bytes, at, options)?),
        FLAG => {
            let name = text(bytes, at)?;
            Ok(options.check_atom(&CfgAtom::Flag(Symbol::intern(name))))
        }
        KEY_VALUE => {
            let key = text(bytes, at)?;
            let value = text(bytes, at)?;
            let atom = CfgAtom::KeyValue { key: Symbol::intern(key), value: Symbol::intern(value) };
            Ok(options.check_atom(&atom))
        }
        _ => Err(Undecodable),
    }
}

fn u16_at(bytes: &[u8], at: &mut usize) -> Result<u16, Undecodable> {
    let word = bytes.get(*at..*at + 2).ok_or(Undecodable)?;
    *at += 2;
    Ok(u16::from_le_bytes([word[0], word[1]]))
}

fn text<'b>(bytes: &'b [u8], at: &mut usize) -> Result<&'b str, Undecodable> {
    let len = u16_at(bytes, at)? as usize;
    let text = bytes.get(*at..*at + len).ok_or(Undecodable)?;
    *at += len;
    core::str::from_utf8(text).map_err(|_| Undecodable)
}
