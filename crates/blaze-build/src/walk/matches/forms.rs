//! Pattern forms of a match by tests (walk/matches.rs) compared by value: a path to a
//! constant, a float literal, a string literal; and the one slot of a name an or-pattern's
//! alternatives each bind. Rules from the Reference, "Patterns":
//! - "Literal patterns": "Literal patterns match exactly the same value as what is created by
//!   the literal." A float literal pattern compares as `==` does (`PartialEq` of floats, IEEE
//!   754 equality): a NaN scrutinee matches no float pattern, and `-0.0` matches `0.0`.
//! - "Path patterns": "Path patterns are irrefutable when they refer to structs or an enum
//!   variant ... They are refutable when they refer to refutable constants or enum variants
//!   for enums with multiple variants." A constant is compared with its evaluated value, leaf
//!   by leaf (a structural match: an ADT's, a tuple's, an array's too; `const_test`); a `&str`
//!   constant by its bytes, as a string literal; any other holding a reference or a pointer is
//!   refused by name.
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
use blaze_typeck::{Def, Name, PNode, PTy, Pat, PatId};

use super::super::{PTR_BITS, Repr, Shape, Walk, malformed};
use crate::layout::Variants;
use super::At;
use crate::BuildError;

/// The bytes a `&str` constant's record points to, for constant pattern `p`: its root is the
/// wide pointer (crate::layout's wide-pointer rule: the address word, then the length), the
/// address word a relocation (crate::data `DataReloc`: "an 8-byte word holding the addend")
/// to an allocation of the same record, whose bytes `addend..addend + len` are the string. A
/// `&str` into a static's bytes is refused by name: no record here holds them.
fn str_const_bytes<B: Allocator>(p: PatId, record: &crate::data::DataRecord<B>) -> Result<&[u8], BuildError> {
    let root = record.allocs.first().ok_or(malformed("a const's record with no allocation"))?;
    let word = |off: usize| -> Result<u64, BuildError> {
        let part = root.bytes.get(off..off + 8).ok_or(malformed("a `&str` constant's record shorter than a wide pointer"))?;
        Ok(part.iter().enumerate().fold(0u64, |v, (i, &byte)| v | (u64::from(byte) << (8 * i))))
    };
    let (addend, len) = (word(0)?, word(8)?);
    let reloc = root.relocs.iter().find(|r| r.offset == 0).ok_or(malformed("a `&str` constant whose address word holds no relocation"))?;
    let crate::data::DataTarget::Local(n) = reloc.target else {
        return Err(BuildError::UnsupportedPat { pat: p, what: "a `&str` constant pointing outside its own record (a static's bytes) as a pattern" });
    };
    let to = record.allocs.get(n as usize).ok_or(malformed("a `&str` constant's relocation past its record's allocations"))?;
    let start = usize::try_from(addend).map_err(|_| malformed("a `&str` constant's addend past a usize"))?;
    let end = usize::try_from(len).ok().and_then(|len| start.checked_add(len)).ok_or(malformed("a `&str` constant's length past a usize"))?;
    to.bytes.get(start..end).ok_or(malformed("a `&str` constant's bytes past its allocation"))
}

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// The tests that the value at `at` equals the constant path pattern `p` names, from the
    /// current block: to `ok` when it does, else to `fail`. `false` (nothing emitted) when `p`
    /// names no constant (a struct or a variant: walk/matches.rs `variant_test`).
    ///
    /// The comparison is structural (the Reference, "Patterns", "Path patterns", and
    /// `[patterns.path.const]`; rustc's `const_to_pat` makes the same pattern): a scalar leaf
    /// compares with the constant's bytes there, a float by `==` (`FloatCond::Oeq`), an enum
    /// by its variant first, then that variant's fields, a struct, tuple or array by every
    /// part. The leaves come from the type's walk row, so padding is never compared, and a
    /// variant's fields are read only once the variant test passed. A `&str` constant is
    /// compared by its bytes, as a string literal pattern is (`bytes_test`). Any other constant
    /// holding a pointer (a `&str` field, a reference) is refused by name: its pointee would be
    /// compared, which no row here holds.
    pub(super) fn const_test(&mut self, p: PatId, at: At, ok: Block, fail: Block) -> Result<bool, BuildError> {
        let Some(Def::Const(item)) = self.pat_def(p) else { return Ok(false) };
        let root = self.repr(at.g).map_err(|error| Self::pat_ty_err(p, error))?;
        let node = self.ground_node(at.g).map_err(|error| Self::pat_ty_err(p, error))?;
        let structural = matches!(node, PNode::Adt(..));
        let refuse = |what| BuildError::UnsupportedPat { pat: p, what };
        // A `&str` constant: compared as a string literal pattern is (`str_test`), by its
        // pointee's bytes. rustc_mir_build const_to_pat.rs `recur` makes a `&str` constant
        // `PatKind::Constant`, tested by `TestKind::Eq` (`<str as PartialEq>::eq`): the same
        // length, then the same bytes.
        let str_ref = match node {
            PNode::Ref(_, inner) => self.ground_node(inner).map_err(|error| Self::pat_ty_err(p, error))? == PNode::Str,
            _ => false,
        };
        if root == Repr::Ptr && !str_ref {
            return Err(refuse("a constant of a reference or pointer type other than `&str` as a pattern (compared by its pointee)"));
        }
        let args = self.pat_const_args(p)?;
        let adts = self.adts;
        let record = adts.const_value(item, &args[..]).ok_or(BuildError::NoPatConstValue { pat: p, item })?;
        record.check().map_err(|_| malformed("a const's record that breaks the data form"))?;
        if str_ref {
            let bytes = str_const_bytes(p, record)?;
            self.bytes_test(p, at, bytes, (true, true), ok, fail)?;
            return Ok(true);
        }
        let bytes = &record.allocs.first().ok_or(malformed("a const's record with no allocation"))?.bytes[..];
        let word = |off: u32, n: u32| -> Result<u128, BuildError> {
            let start = off as usize;
            let part = bytes.get(start..start + n as usize).ok_or(malformed("a const's bytes shorter than its type"))?;
            Ok(part.iter().enumerate().fold(0u128, |v, (i, &byte)| v | (u128::from(byte) << (8 * i))))
        };
        let mut all: Option<Value> = None;
        let mut todo: Vec<(Repr, u32), A> = Vec::new_in(self.alloc);
        todo.push((root, 0));
        while let Some((repr, off)) = todo.pop() {
            let place = i32::try_from(off).ok().and_then(|o| at.at.1.checked_add(o)).ok_or(refuse("a constant pattern past an `i32` offset"))?;
            let hit = match repr {
                Repr::None => continue,
                Repr::Int { ty, .. } | Repr::Float(ty) => {
                    let k = word(off, ty.bits(PTR_BITS).div_ceil(8))?;
                    let attrs = self.mem(repr);
                    let x = self.b.load(ty, Flags::NONE, at.at.0, place, attrs);
                    if let Repr::Float(_) = repr {
                        // rustc's `const_to_pat` refuses a NaN ("cannot use NaN in patterns"):
                        // it equals nothing, so the arm could never match.
                        let width = ty.bits(PTR_BITS);
                        let exp = match width {
                            16 => 5,
                            32 => 8,
                            64 => 11,
                            _ => 15,
                        };
                        let mantissa = width - 1 - exp;
                        let exp_mask = ((1u128 << exp) - 1) << mantissa;
                        if k & exp_mask == exp_mask && k & ((1u128 << mantissa) - 1) != 0 {
                            return Err(refuse("a NaN constant as a pattern (rustc: \"cannot use NaN in patterns\")"));
                        }
                        let k = self.b.fconst(ty, k);
                        self.b.fcmp(FloatCond::Oeq, x, k)
                    } else {
                        let k = self.b.iconst(ty, k);
                        self.b.icmp(IntCond::Eq, x, k)
                    }
                }
                Repr::Bool => {
                    let k = word(off, 1)?;
                    let attrs = self.mem(repr);
                    let x = self.b.load(Ty::I8, Flags::NONE, at.at.0, place, attrs);
                    let k = self.b.iconst(Ty::I8, k);
                    self.b.icmp(IntCond::Eq, x, k)
                }
                Repr::Ptr => {
                    return Err(refuse(if structural {
                        "a structural match of an ADT constant holding a reference or a pointer (compared by its pointee)"
                    } else {
                        "a constant holding a reference or a pointer as a pattern (compared by its pointee)"
                    }));
                }
                Repr::Agg(l) => {
                    if self.layouts[l as usize].is_union {
                        return Err(malformed("a constant pattern of a union type (E0158 refuses it)"));
                    }
                    match self.layouts[l as usize].shape {
                        Shape::Fields { first, len } => {
                            for i in first..first + len {
                                let (_, at_off, part) = self.layout_fields[i as usize];
                                todo.push((part, off + at_off));
                            }
                        }
                        Shape::Array { elem, len, stride } => {
                            for i in 0..len {
                                todo.push((elem, off + i * stride));
                            }
                        }
                        Shape::Enum { id, .. } => {
                            // The constant's variant, decoded from its tag bytes as `ls` encodes it.
                            let variant = match self.ls.read_tag(id) {
                                None => self.ls.variant_of_tag(id, 0),
                                Some((tag_at, tag)) => {
                                    let tag_at = u32::try_from(tag_at).map_err(|_| malformed("an enum's tag past a u32 offset"))?;
                                    let bits = word(off + tag_at, tag.prim.bits() / 8)?;
                                    self.ls.variant_of_tag(id, bits)
                                }
                            }
                            .ok_or(malformed("a constant whose tag names no variant of its enum"))?;
                            if let Variants::Multiple { .. } = self.ls.get(id).variants {
                                // Everything compared so far, then the variant, before any of
                                // its fields is read.
                                if let Some(c) = all.take() {
                                    let mid = self.create();
                                    self.br(c, (mid, &[]), (fail, &[]));
                                    self.begin(mid);
                                }
                                let d = self.read_discriminant(id, (at.at.0, place))?;
                                let case = self.discriminant_case(id, variant)?;
                                let k = self.b.iconst(Ty::I64, case);
                                all = Some(self.b.icmp(IntCond::Eq, d, k));
                            }
                            let (first, len, _) = self.variant_rows(l, variant)?;
                            for i in first..first + len {
                                let (_, at_off, part) = self.layout_fields[i as usize];
                                todo.push((part, off + at_off));
                            }
                            continue;
                        }
                    }
                    continue;
                }
            };
            all = Some(match all {
                Some(prev) => self.b.binary(Op::And, Flags::NONE, prev, hit),
                None => hit,
            });
        }
        match all {
            Some(c) => self.br(c, (ok, &[]), (fail, &[])),
            // A constant of no bytes (a unit struct): the type alone matches.
            None => self.jump(ok, &[]),
        }
        Ok(true)
    }

    /// The generic arguments the check recorded for constant pattern `p`
    /// (`Checked::pat_const_args_of`: an inherent impl's constant, `U256::ZERO`), grounded:
    /// they never name a generic parameter (E0158), so grounding only resolves them. Empty for
    /// a constant with no generics.
    fn pat_const_args(&mut self, p: PatId) -> Result<Vec<PTy, A>, BuildError> {
        let Some(list) = self.checked.pat_const_args_of(p) else {
            return Ok(Vec::new_in(self.alloc));
        };
        let tys = self.types.list(list).map_err(|_| malformed("a constant pattern's arguments past the record's types"))?;
        let mut grounded = Vec::with_capacity_in(tys.len(), self.alloc);
        for &t in tys {
            grounded.push(self.ground_ty(t).map_err(|error| Self::pat_ty_err(p, error))?);
        }
        Ok(grounded)
    }

    /// The value of const `item`, named by path pattern `p` (a range's end), as `width` bits:
    /// its evaluated record's root bytes, little-endian (walk.rs `const_value` reads a scalar
    /// the same way).
    pub(super) fn const_bits(&mut self, p: PatId, item: blaze_typeck::ItemKey, width: u32) -> Result<u128, BuildError> {
        let args = self.pat_const_args(p)?;
        let adts = self.adts;
        let record = adts.const_value(item, &args[..]).ok_or(BuildError::NoPatConstValue { pat: p, item })?;
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
    ///
    /// A byte string literal pattern (`b".."`, the Reference, "Literal patterns": its type is
    /// `&[u8; N]`, or `&[u8]` / `[u8; N]` where the scrutinee is one) the same way over a
    /// `&[u8]`; over a `&[u8; N]` or a `[u8; N]` in place the type fixes the length, so only
    /// the bytes are compared.
    pub(super) fn str_test(&mut self, p: PatId, at: At, ok: Block, fail: Block) -> Result<(), BuildError> {
        let byte_str = matches!(self.pat(p)?, Pat::Lit(blaze_typeck::Lit::ByteStr(_)));
        let refuse = BuildError::UnsupportedPat {
            pat: p,
            what: if byte_str {
                "a byte string literal pattern over other than `&[u8]`, `&[u8; N]` or `[u8; N]`"
            } else {
                "a string literal pattern over other than `&str`"
            },
        };
        let node = |w: &mut Self, g| w.ground_node(g).map_err(|error| Self::pat_ty_err(p, error));
        // Where the bytes are: behind the pointer at `at` (with the length after it for a
        // wide one), or at `at` itself (an array in place).
        let (behind, wide) = match node(self, at.g)? {
            PNode::Ref(_, inner) => match node(self, inner)? {
                PNode::Str if !byte_str => (true, true),
                PNode::Slice(e) if byte_str && node(self, e)? == PNode::Uint(blaze_typeck::UintTy::U8) => (true, true),
                PNode::Array(e, _) if byte_str && node(self, e)? == PNode::Uint(blaze_typeck::UintTy::U8) => (true, false),
                _ => return Err(refuse),
            },
            PNode::Array(e, _) if byte_str && node(self, e)? == PNode::Uint(blaze_typeck::UintTy::U8) => (false, false),
            _ => return Err(refuse),
        };
        let adts = self.adts;
        let bytes = adts.str_pat(self.body.owner, p).ok_or(BuildError::NoPatValue { pat: p })?;
        self.bytes_test(p, at, bytes, (behind, wide), ok, fail)
    }

    /// The tests that the value at `at` holds `bytes`, for pattern `p`: behind the pointer at
    /// `at` (`behind`) with its length in the word after it (`wide`), or in place: the length
    /// first when it is a wide pointer's, then the bytes a word at a time; to `ok` when all are
    /// equal, else to `fail`. No byte is read before the length test passed. The one compare of
    /// a string literal pattern (`str_test`) and of a `&str` constant (`const_test`).
    fn bytes_test(&mut self, p: PatId, at: At, bytes: &[u8], (behind, wide): (bool, bool), ok: Block, fail: Block) -> Result<(), BuildError> {
        if wide {
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
        } else if bytes.is_empty() {
            // `[u8; 0]`: the type alone matches.
            self.jump(ok, &[]);
            return Ok(());
        }
        let (ptr, base) = if behind {
            (self.b.load(Ty::PTR, Flags::NONE, at.at.0, at.at.1, MemAttrs::aligned(3)), 0)
        } else {
            at.at
        };
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
            let off = i32::try_from(i)
                .ok()
                .and_then(|i| base.checked_add(i))
                .ok_or(BuildError::UnsupportedPat { pat: p, what: "a string literal pattern past an `i32` offset" })?;
            // The bytes of a `str` or a `[u8]` have no alignment past one.
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
        let mut todo: Vec<PatId, A> = Vec::new_in(self.alloc);
        todo.push(root);
        // Children are pushed last first, so they pop in source order.
        while let Some(q) = todo.pop() {
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
                    for &s in subs.iter().rev() {
                        todo.push(s);
                    }
                }
                Pat::Struct { fields, .. } | Pat::Record { fields, .. } => {
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
