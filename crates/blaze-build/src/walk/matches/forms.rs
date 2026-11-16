//! Pattern forms of a match by tests (walk/matches.rs) compared by value: a path to a
//! constant, a float literal, a string literal; and the one slot of a name an or-pattern's
//! alternatives each bind. Rules from the Reference, "Patterns":
//! - "Literal patterns": "Literal patterns match exactly the same value as what is created by
//!   the literal." A float literal pattern compares as `==` does (`PartialEq` of floats, IEEE
//!   754 equality): a NaN scrutinee matches no float pattern, and `-0.0` matches `0.0`.
//! - "Path patterns": "Path patterns are irrefutable when they refer to structs or an enum
//!   variant ... They are refutable when they refer to refutable constants or enum variants
//!   for enums with multiple variants." A constant of an integer, `char` or `bool` type is
//!   compared with its evaluated value; a constant of a struct or enum type (a structural
//!   match) is refused by name.
//! - "Or-patterns": "every binding in each pattern [alternative] must have the same type and
//!   binding mode" and each alternative binds the same set of names. The lowering resolves a
//!   use of such a name to its first binding in the pattern (blaze-lower machine.rs
//!   `activate`: "one binding, the first's"), so every alternative stores into the first
//!   binding's slot.
//!
//! A string pattern is the length test, then the bytes compared a word at a time against
//! constants (no call into the library, no data row): the literal's length is known here, so
//! the compare is `len / 8` loads and compares, with no loop-carried state.

use core::alloc::Allocator;

use blaze_lir::builder::MemAttrs;
use blaze_lir::{Block, Flags, FloatCond, IntCond, Op, Ty, Value};
use blaze_own::Vec;
use blaze_typeck::{Def, Name, PNode, Pat, PatId};

use super::super::{PTR_BITS, Repr, Walk, malformed};
use super::At;
use crate::BuildError;

impl<A: Allocator + Clone> Walk<'_, '_, '_, A> {
    /// The test that the value at `at` equals the constant path pattern `p` names; none when
    /// `p` names no constant (a struct or a variant: walk/matches.rs `variant_test`).
    pub(super) fn const_test(&mut self, p: PatId, at: At) -> Result<Option<Value>, BuildError> {
        let Some(Def::Const(item)) = self.pat_def(p) else { return Ok(None) };
        let repr = self.repr(at.g).map_err(|error| Self::pat_ty_err(p, error))?;
        match repr {
            Repr::Int { .. } | Repr::Bool => {}
            Repr::Float(_) => {
                return Err(BuildError::UnsupportedPat { pat: p, what: "a constant of a float type as a pattern" });
            }
            _ => {
                let what = match self.ground_node(at.g).map_err(|error| Self::pat_ty_err(p, error))? {
                    PNode::Adt(..) => "a structural match of an ADT constant",
                    _ => "a constant of other than an integer, a `char` or a `bool` type as a pattern (a tuple, an array, a reference)",
                };
                return Err(BuildError::UnsupportedPat { pat: p, what });
            }
        }
        let (x, ty, _) = self.scalar_at(p, at)?;
        let v = self.const_bits(p, item, ty.bits(PTR_BITS))?;
        let k = self.b.iconst(ty, v);
        Ok(Some(self.b.icmp(IntCond::Eq, x, k)))
    }

    /// The value of const `item`, named by path pattern `p`, as `width` bits: its evaluated
    /// record's root bytes, little-endian (walk.rs `const_value` reads a scalar the same way).
    pub(super) fn const_bits(&mut self, p: PatId, item: blaze_typeck::ItemKey, width: u32) -> Result<u128, BuildError> {
        let adts = self.adts;
        let record = adts.const_value(item, &[]).ok_or(BuildError::NoPatConstValue { pat: p, item })?;
        record.check().map_err(|_| malformed("a const's record that breaks the data form"))?;
        let root = record.allocs.first().ok_or(malformed("a const's record with no allocation"))?;
        if !root.relocs.is_empty() {
            return Err(malformed("a scalar constant's record that holds a relocation"));
        }
        let n = width.div_ceil(8) as usize;
        let bytes = root.bytes.get(..n).ok_or(malformed("a const's bytes shorter than its type"))?;
        let mut value = 0u128;
        for (i, &byte) in bytes.iter().enumerate() {
            value |= u128::from(byte) << (8 * i);
        }
        Ok(value)
    }

    /// The test that the float at `at` equals float literal pattern `p` (`-lit` too) by IEEE
    /// equality (`FloatCond::Oeq`: ordered, so a NaN is equal to nothing).
    pub(super) fn float_test(&mut self, p: PatId, at: At) -> Result<Value, BuildError> {
        let repr = self.repr(at.g).map_err(|error| Self::pat_ty_err(p, error))?;
        let Repr::Float(ty) = repr else { return Err(malformed("a float literal pattern over other than a float")) };
        let width = ty.bits(PTR_BITS);
        let adts = self.adts;
        let mut bits = adts.float_pat(self.body.owner, p).ok_or(BuildError::NoPatValue { pat: p })?;
        if let Pat::NegLit(_) = self.pat(p)? {
            bits ^= 1u128 << (width - 1);
        }
        let attrs = self.mem(repr);
        let x = self.b.load(ty, Flags::NONE, at.at.0, at.at.1, attrs);
        let k = self.b.fconst(ty, bits);
        Ok(self.b.fcmp(FloatCond::Oeq, x, k))
    }

    /// The tests that the `&str` at `at` (the wide pointer's memory) holds the bytes of string
    /// literal pattern `p`, from the current block: the length, then the bytes, a word at a
    /// time; to `ok` when all are equal, else to `fail`. No byte is read before the length
    /// test passed.
    pub(super) fn str_test(&mut self, p: PatId, at: At, ok: Block, fail: Block) -> Result<(), BuildError> {
        let refuse = BuildError::UnsupportedPat { pat: p, what: "a string literal pattern over other than `&str`" };
        let PNode::Ref(_, inner) = self.ground_node(at.g).map_err(|error| Self::pat_ty_err(p, error))? else { return Err(refuse) };
        let PNode::Str = self.ground_node(inner).map_err(|error| Self::pat_ty_err(p, error))? else { return Err(refuse) };
        let adts = self.adts;
        let bytes = adts.str_pat(self.body.owner, p).ok_or(BuildError::NoPatValue { pat: p })?.as_bytes();
        let len = self.b.load(Ty::I64, Flags::NONE, at.at.0, at.at.1 + 8, MemAttrs::aligned(3));
        let n = self.b.iconst(Ty::I64, bytes.len() as u128);
        let same_len = self.b.icmp(IntCond::Eq, len, n);
        if bytes.is_empty() {
            self.br(same_len, (ok, &[]), (fail, &[]));
            return Ok(());
        }
        let mid = self.create();
        self.br(same_len, (mid, &[]), (fail, &[]));
        self.begin(mid);
        let ptr = self.b.load(Ty::PTR, Flags::NONE, at.at.0, at.at.1, MemAttrs::aligned(3));
        let mut all: Option<Value> = None;
        let mut i = 0usize;
        while i < bytes.len() {
            let (ty, w) = match bytes.len() - i {
                8.. => (Ty::I64, 8),
                4..=7 => (Ty::I32, 4),
                2 | 3 => (Ty::I16, 2),
                _ => (Ty::I8, 1),
            };
            let mut word = 0u128;
            for (j, &byte) in bytes[i..i + w].iter().enumerate() {
                word |= u128::from(byte) << (8 * j);
            }
            let off = i32::try_from(i).map_err(|_| BuildError::UnsupportedPat { pat: p, what: "a string literal pattern past an `i32` offset" })?;
            // The bytes of a `str` have no alignment past one.
            let x = self.b.load(ty, Flags::NONE, ptr, off, MemAttrs::aligned(0));
            let k = self.b.iconst(ty, word);
            let hit = self.b.icmp(IntCond::Eq, x, k);
            all = Some(match all {
                Some(prev) => self.b.binary(Op::And, Flags::NONE, prev, hit),
                None => hit,
            });
            i += w;
        }
        let all = all.ok_or(malformed("a non-empty string pattern with no word compared"))?;
        self.br(all, (ok, &[]), (fail, &[]));
        Ok(())
    }

    /// Each name `root` binds, with its first binding pattern in source order (pre-order, left
    /// to right): the slot every or-alternative's binding of that name stores into.
    pub(super) fn first_binds(&self, root: PatId, out: &mut Vec<(Name, PatId), A>) -> Result<(), BuildError> {
        let mut todo: Vec<PatId, A> = Vec::new_in(self.alloc.clone());
        todo.push(root);
        while let Some(q) = todo.pop() {
            // Children pushed last first, so they pop in source order.
            let mut push_list = |todo: &mut Vec<PatId, A>, list: &[PatId]| {
                for &s in list.iter().rev() {
                    todo.push(s);
                }
            };
            match self.pat(q)? {
                Pat::Bind { name, sub, .. } => {
                    if !out.iter().any(|&(n, _)| n == name) {
                        out.push((name, q));
                    }
                    if let Some(s) = sub {
                        todo.push(s);
                    }
                }
                Pat::Ref { inner, .. } => todo.push(inner),
                Pat::Tuple { elems, .. } | Pat::TupleStruct { elems, .. } | Pat::Slice { elems, .. } | Pat::Or { alts: elems } => {
                    let subs = self.body.pat_lists.get(elems.range()).ok_or(malformed("a pattern's elements past `pat_lists`"))?;
                    push_list(&mut todo, subs);
                }
                Pat::Struct { fields, .. } => {
                    let fps = self.body.field_pats.get(fields.range()).ok_or(malformed("a struct pattern's fields past `field_pats`"))?;
                    for fp in fps.iter().rev() {
                        todo.push(fp.pat);
                    }
                }
                Pat::Missing | Pat::Wild | Pat::Lit(_) | Pat::NegLit(_) | Pat::Path(_) | Pat::Range { .. } => {}
            }
        }
        Ok(())
    }

    /// The pattern a binding `p` named `name` stores into: its name's first binding in
    /// `alias` (`first_binds`), else `p` itself.
    pub(super) fn bind_target(alias: &[(Name, PatId)], name: Name, p: PatId) -> PatId {
        alias.iter().find(|&&(n, _)| n == name).map_or(p, |&(_, first)| first)
    }
}
