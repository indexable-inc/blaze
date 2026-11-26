//! Patterns: their tests (`match`, `if let`) and their bindings (`let`, parameters, arms).
//!
//! A `match` is lowered arm by arm: each arm's pattern is a chain of tests, each a one-case
//! `switch` whose `otherwise` is the next arm's tests (the Reference, "Match expressions": "the
//! first arm whose pattern matches is chosen"). A decision tree that shares tests across arms is
//! not built yet. An enum variant is tested by `switch` on `discr place`; an integer, `char` or
//! `bool` literal by `switch` on the place's value.
//!
//! Default binding modes (the Reference, "Binding modes"): where a non-reference pattern meets
//! a reference, the reference is dereferenced. The record says where: a structural pattern's
//! type (`Checked::pat_ty`) is the type it matches, so a place of type `&T` met by a pattern of
//! type `T` is dereferenced; a binding whose variable's type (`Checked::local_ty`) is `&T` over
//! a place of type `T` binds by reference.

use core::alloc::Allocator;

use blaze_body::{
    BinOp as BBin, Block, BlockKind, BorrowKind, ConstDecl, ConstKind, Local, LocalDecl, LocalKind, Operand, Place,
    Proj, Role, Span, StmtKind, Value,
};
use blaze_own::Vec;
use blaze_typeck::{
    Def, Expr, ExprId, FloatTy, IntTy, ItemKey, Lit, List, Mutbl, Name, PNode, PTy, Pat, PatId, UintTy, UnionPat,
};

use crate::scope::Binding;
use crate::{At, BodyFacts, Construct, DiscrInt, Lower, LowerError, MOST_POSITIONS};

/// Most implicit dereferences one pattern takes.
const MOST_PEELS: u32 = 64;

/// Most or-free expansions one binding or-pattern expands to.
const MOST_OR_ALTS: u32 = 64;

const EMPTY_LIST: List = List { start: 0, len: 0 };

/// One or-pattern choice during expansion: `(or_pat, chosen_alt_index, total_alts)`.
type OrChoice = (PatId, u32, u32);

impl<F: BodyFacts, A: Allocator + Copy> Lower<'_, '_, '_, F, A> {
    /// A binding's local, its storage begun.
    pub(crate) fn binding_local(&mut self, p: PatId, ty: PTy) -> Result<(Local, Place), LowerError> {
        let mutbl = match self.pat(p)? {
            Pat::Bind { ann, .. } if ann.mutable => Mutbl::Mut,
            _ => Mutbl::Not,
        };
        let cty = self.canonical_ty(ty, At::Pat(p))?;
        let local = self.b.new_local(LocalDecl { ty: cty, kind: LocalKind::Binding(p), mutbl });
        let place = self.b.local_place(local);
        let span = self.pat_span(p);
        self.b.push(StmtKind::StorageLive(local), span);
        Ok((local, place))
    }

    /// Whether `p` is a pattern that matches a value: not a plain binding, wildcard or
    /// reference pattern, which take their place as it is.
    fn structural(&self, p: PatId) -> Result<bool, LowerError> {
        Ok(match self.pat(p)? {
            Pat::Wild | Pat::Missing | Pat::Ref { .. } => false,
            Pat::Bind { .. } => self.ck.union_pat_of(p).is_some(),
            _ => true,
        })
    }

    /// The place and type structural pattern `p` matches: `place` dereferenced while its type
    /// is a reference and not the pattern's.
    fn peel(&mut self, p: PatId, mut place: Place, mut ty: PTy) -> Result<(Place, PTy), LowerError> {
        if !self.structural(p)? {
            return Ok((place, ty));
        }
        let want = self.pat_ty(p)?;
        let mut n = 0u32;
        while ty != want {
            n += 1;
            if n > MOST_PEELS {
                return self.refuse_pat(Construct::Bound, p);
            }
            match self.node(ty, At::Pat(p))? {
                PNode::Ref(_, t) => {
                    place = self.b.project(place, Proj::Deref);
                    ty = t;
                }
                _ => break,
            }
        }
        Ok((place, ty))
    }

    /// The (adt, variant) a constructor pattern names.
    fn ctor_of(&self, p: PatId) -> Result<(ItemKey, u32), LowerError> {
        match self.pat_def(p) {
            Some(Def::Ctor { adt, variant, .. }) => Ok((adt, variant)),
            Some(Def::Adt(k)) => Ok((k, 0)),
            _ => self.refuse_pat(Construct::Malformed, p),
        }
    }

    /// The fields of variant `variant` of `adt` at `place`: through `Downcast` for an enum; a
    /// field of a type with a `Drop` impl is marked.
    fn variant_base(&mut self, adt: ItemKey, variant: u32, place: Place, p: PatId) -> Result<(Place, bool), LowerError> {
        let f = self.adt_facts(adt, At::Pat(p))?;
        if f.union {
            return self.refuse_pat(Construct::Union, p);
        }
        if f.is_enum {
            Ok((self.b.project(place, Proj::Downcast(variant)), f.drop_impl))
        } else {
            Ok((place, f.drop_impl))
        }
    }

    /// Each sub-pattern of a positional list (`elems`, a `..` at `rest`) over `n` fields, with
    /// its field index.
    fn positional(&self, elems: List, rest: Option<u32>, n: u32, p: PatId) -> Result<Vec<(u32, PatId), A>, LowerError> {
        let subs = self.pats_in(elems, p)?;
        let mut out = Vec::new_in(self.alloc);
        let len = subs.len() as u32;
        if len > n {
            return self.refuse_pat(Construct::Malformed, p);
        }
        for (i, &s) in subs.iter().enumerate() {
            let i = i as u32;
            let index = match rest {
                Some(r) if i >= r => n - (len - i),
                _ => i,
            };
            out.push((index, s));
        }
        Ok(out)
    }

    /// The fields a tuple, tuple struct or struct pattern names, each with its index, its
    /// sub-pattern and its type; and the base place their projections start from.
    fn sub_fields(&mut self, p: PatId, place: Place, ty: PTy) -> Result<(Place, bool, Vec<(u32, PatId, PTy), A>), LowerError> {
        let mut out = Vec::new_in(self.alloc);
        match self.pat(p)? {
            Pat::Tuple { elems, rest } => {
                let PNode::Tuple(l) = self.node(ty, At::Pat(p))? else { return self.refuse_pat(Construct::Malformed, p) };
                let tys = self.list(l, At::Pat(p))?;
                for (i, s) in self.positional(elems, rest, tys.len() as u32, p)? {
                    let Some(&ft) = tys.get(i as usize) else {
                        return self.refuse_pat(Construct::Malformed, p);
                    };
                    out.push((i, s, ft));
                }
                Ok((place, false, out))
            }
            Pat::TupleStruct { elems, rest, .. } => {
                let (adt, variant) = self.ctor_of(p)?;
                let Some(n) = self.facts.field_count(adt, variant) else { return self.refuse_pat(Construct::UnknownField, p) };
                let (base, marked) = self.variant_base(adt, variant, place, p)?;
                for (i, s) in self.positional(elems, rest, n, p)? {
                    out.push((i, s, self.field_ty_of(s)?));
                }
                Ok((base, marked, out))
            }
            Pat::Struct { fields, .. } => {
                let (adt, variant) = self.ctor_of(p)?;
                let (base, marked) = self.variant_base(adt, variant, place, p)?;
                for fp in self.field_pats_in(fields, p)? {
                    let Some(i) = self.facts.field(adt, variant, fp.name) else { return self.refuse_pat(Construct::UnknownField, p) };
                    out.push((i, fp.pat, self.field_ty_of(fp.pat)?));
                }
                Ok((base, marked, out))
            }
            Pat::Record { fields, .. } => {
                let PNode::Record(schema, flist) = self.node(ty, At::Pat(p))? else {
                    return self.refuse_pat(Construct::Malformed, p);
                };
                let tys = self.list(flist, At::Pat(p))?;
                for fp in self.field_pats_in(fields, p)? {
                    let Some(i) = self.tys.record_field_index(schema, fp.name) else {
                        return self.refuse_pat(Construct::UnknownField, fp.pat);
                    };
                    let Ok(ix) = usize::try_from(i) else { return self.refuse_pat(Construct::Malformed, fp.pat) };
                    let Some(&ft) = tys.get(ix) else { return self.refuse_pat(Construct::Malformed, fp.pat) };
                    out.push((i, fp.pat, ft));
                }
                Ok((place, false, out))
            }
            _ => Ok((place, false, out)),
        }
    }

    /// The type of the ADT field sub-pattern `s` matches. The record has no field types of an
    /// ADT at its arguments, so this is the sub-pattern's own type: exact unless match
    /// ergonomics dereferences at that field (said in the crate's report as a gap).
    fn field_ty_of(&self, s: PatId) -> Result<PTy, LowerError> {
        self.pat_ty(s)
    }

    /// The tests of pattern `p` against `place` (of type `ty`) in the open block; a failed test
    /// goes to `fail`; on success the open block is where the pattern matched. `e` is the
    /// expression the tests are made for (their scratch locals name it).
    pub(crate) fn test_pat(&mut self, p: PatId, place: Place, ty: PTy, fail: Block, e: ExprId) -> Result<(), LowerError> {
        self.test_pat_opt(p, place, ty, fail, e, None)
    }

    fn test_pat_opt(
        &mut self,
        p: PatId,
        place: Place,
        ty: PTy,
        fail: Block,
        e: ExprId,
        choice: Option<&[OrChoice]>,
    ) -> Result<(), LowerError> {
        self.depth += 1;
        if self.depth > crate::MOST_DEPTH {
            return self.refuse_pat(Construct::Depth, p);
        }
        let r = self.test_inner(p, place, ty, fail, e, choice);
        self.depth -= 1;
        r
    }

    fn test_inner(
        &mut self,
        p: PatId,
        place: Place,
        ty: PTy,
        fail: Block,
        e: ExprId,
        choice: Option<&[OrChoice]>,
    ) -> Result<(), LowerError> {
        let (place, ty) = self.peel(p, place, ty)?;
        let span = self.pat_span(p);
        match self.pat(p)? {
            Pat::Missing => self.refuse_pat(Construct::Malformed, p),
            Pat::Wild => Ok(()),
            Pat::Bind { sub, .. } => match self.ck.union_pat_of(p) {
                None => match sub {
                    Some(s) => self.test_pat_opt(s, place, ty, fail, e, choice),
                    None => Ok(()),
                },
                Some(UnionPat::Variant(v)) => {
                    let PNode::Union(ul) = self.node(ty, At::Pat(p))? else {
                        return self.refuse_pat(Construct::Malformed, p);
                    };
                    if v >= ul.len {
                        return self.refuse_pat(Construct::Malformed, p);
                    }
                    if ul.len > 1 {
                        let dty = self.find_ty(PNode::Int(IntTy::Isize), At::Pat(p))?;
                        let discr = self.scratch(e, dty);
                        self.b.assign(discr, Value::Discriminant(place), span);
                        self.switch_eq(Operand::Move(discr), u128::from(v), fail, span);
                    }
                    match sub {
                        Some(s) => {
                            let Ok(v_idx) = usize::try_from(v) else { return self.refuse_pat(Construct::Malformed, p) };
                            let Some(&member_ty) = self.list(ul, At::Pat(p))?.get(v_idx) else {
                                return self.refuse_pat(Construct::Malformed, p);
                            };
                            let dp = self.b.project(place, Proj::Downcast(v));
                            let fp = self.b.project(dp, Proj::Field { index: 0, ty: member_ty });
                            self.test_pat_opt(s, fp, member_ty, fail, e, choice)
                        }
                        None => Ok(()),
                    }
                }
                Some(UnionPat::Residual) => {
                    let lt = self.local_ty(p)?;
                    let dst_ty = match self.node(lt, At::Pat(p))? {
                        PNode::Ref(_, inner) if matches!(self.node(inner, At::Pat(p))?, PNode::Union(_)) => inner,
                        PNode::Union(_) => lt,
                        _ => return self.refuse_pat(Construct::Malformed, p),
                    };
                    let PNode::Union(src_l) = self.node(ty, At::Pat(p))? else {
                        return self.refuse_pat(Construct::Malformed, p);
                    };
                    let PNode::Union(dst_l) = self.node(dst_ty, At::Pat(p))? else {
                        return self.refuse_pat(Construct::Malformed, p);
                    };
                    let mut src_members: Vec<PTy, A> = Vec::new_in(self.alloc);
                    src_members.extend_from_slice(self.list(src_l, At::Pat(p))?);
                    let mut dst_members: Vec<PTy, A> = Vec::new_in(self.alloc);
                    dst_members.extend_from_slice(self.list(dst_l, At::Pat(p))?);
                    let mut matching: Vec<u32, A> = Vec::new_in(self.alloc);
                    for (i_idx, &sm) in src_members.iter().enumerate() {
                        if dst_members.scan_for(&sm).is_some() {
                            let Ok(i) = u32::try_from(i_idx) else { return self.refuse_pat(Construct::Malformed, p) };
                            matching.push(i);
                        }
                    }
                    if matching.is_empty() {
                        self.goto(fail, span);
                        self.dead_block();
                        return Ok(());
                    }
                    if matching.len() < src_members.len() {
                        let dty = self.find_ty(PNode::Int(IntTy::Isize), At::Pat(p))?;
                        let discr = self.scratch(e, dty);
                        self.b.assign(discr, Value::Discriminant(place), span);
                        let ok = self.b.new_block(BlockKind::Normal);
                        let mut cases_vec: Vec<(u128, Block), A> = Vec::new_in(self.alloc);
                        for &i in matching.iter() {
                            cases_vec.push((u128::from(i), ok));
                        }
                        let cases = self.b.cases(&cases_vec);
                        self.b.terminate(StmtKind::Switch { on: Operand::Move(discr), cases, otherwise: fail }, span);
                        self.b.start(ok);
                    }
                    match sub {
                        Some(s) => {
                            let tp = self.temp(e, dst_ty, Role::Value)?;
                            self.remap_union(place, ty, dst_ty, tp, e, span, At::Pat(p))?;
                            self.test_pat_opt(s, tp, dst_ty, fail, e, choice)
                        }
                        None => Ok(()),
                    }
                }
            },
            Pat::Ref { inner, .. } => {
                let it = self.pointee(ty, At::Pat(p))?;
                let ip = self.b.project(place, Proj::Deref);
                self.test_pat_opt(inner, ip, it, fail, e, choice)
            }
            Pat::Lit(l) | Pat::NegLit(l) => {
                let negated = matches!(self.pat(p)?, Pat::NegLit(_));
                match l {
                    Lit::Bool(b) => {
                        self.switch_eq(Operand::Copy(place), u128::from(b), fail, span);
                        Ok(())
                    }
                    Lit::Int(_) | Lit::Char => {
                        let Some(bits) = self.facts.int_pat(p) else {
                            return self.refuse_pat(Construct::NoLiteralValue, p);
                        };
                        self.switch_eq(Operand::Copy(place), bits, fail, span);
                        Ok(())
                    }
                    Lit::Float(_) => self.test_float_pat(p, negated, place, ty, fail, e, span),
                    Lit::Str | Lit::ByteStr(_) => self.test_str_pat(p, matches!(l, Lit::ByteStr(_)), place, ty, fail, e, span),
                    Lit::CStr => self.refuse_pat(Construct::ValuePattern, p),
                }
            }
            Pat::Path(_) => match self.pat_def(p) {
                Some(Def::Ctor { adt, variant, .. }) => self.variant_test(adt, variant, place, fail, p, e),
                Some(Def::Const(key)) => self.test_const_pat(p, key, place, ty, fail, e, span),
                _ => self.refuse_pat(Construct::Malformed, p),
            },
            Pat::Tuple { .. } | Pat::TupleStruct { .. } | Pat::Struct { .. } | Pat::Record { .. } => {
                if let Pat::TupleStruct { .. } | Pat::Struct { .. } = self.pat(p)? {
                    let (adt, variant) = self.ctor_of(p)?;
                    self.variant_test(adt, variant, place, fail, p, e)?;
                }
                let (base, marked, subs) = self.sub_fields(p, place, ty)?;
                for &(index, s, fty) in subs.iter() {
                    let fp = self.b.project_marked(base, Proj::Field { index, ty: fty }, marked);
                    self.test_pat_opt(s, fp, fty, fail, e, choice)?;
                }
                Ok(())
            }
            Pat::Or { alts } => {
                let list = self.pats_in(alts, p)?;
                if let Some(ch) = choice {
                    let Some(&(_, alt_i, _)) = ch.iter().find(|&&(q, _, _)| q == p) else {
                        return self.refuse_pat(Construct::Malformed, p);
                    };
                    let Ok(ix) = usize::try_from(alt_i) else {
                        return self.refuse_pat(Construct::Malformed, p);
                    };
                    let Some(&alt) = list.get(ix) else {
                        return self.refuse_pat(Construct::Malformed, p);
                    };
                    return self.test_pat_opt(alt, place, ty, fail, e, Some(ch));
                }
                if self.pat_has_bindings(p)? {
                    return self.refuse_pat(Construct::OrPattern, p);
                }
                if list.is_empty() {
                    self.goto(fail, span);
                    self.dead_block();
                    return Ok(());
                }
                let ok = self.b.new_block(BlockKind::Normal);
                let last = list.len() - 1;
                for (i, &alt) in list.iter().enumerate() {
                    if i == last {
                        self.test_pat_opt(alt, place, ty, fail, e, None)?;
                        self.goto(ok, span);
                    } else {
                        let next = self.b.new_block(BlockKind::Normal);
                        self.test_pat_opt(alt, place, ty, next, e, None)?;
                        self.goto(ok, span);
                        self.b.start(next);
                    }
                }
                self.b.start(ok);
                Ok(())
            }
            Pat::Slice { elems, rest } => self.test_slice_pat(p, elems, rest, place, ty, fail, e, choice),
            Pat::Range { lo, hi, inclusive } => self.test_range_pat(p, lo, hi, inclusive, place, ty, fail, e),
        }
    }

    /// Tests a float literal pattern against `place` via IEEE-754 ordered equality (`==`).
    fn test_float_pat(
        &mut self,
        p: PatId,
        negated: bool,
        place: Place,
        ty: PTy,
        fail: Block,
        e: ExprId,
        span: Span,
    ) -> Result<(), LowerError> {
        let Some(mut bits) = self.facts.float_pat(p) else {
            return self.refuse_pat(Construct::ValuePattern, p);
        };
        if negated {
            let sign_bit = match self.node(ty, At::Pat(p))? {
                PNode::Float(FloatTy::F16) => 15u32,
                PNode::Float(FloatTy::F32) => 31u32,
                PNode::Float(FloatTy::F64) => 63u32,
                PNode::Float(FloatTy::F128) => 127u32,
                _ => return self.refuse_pat(Construct::Malformed, p),
            };
            bits ^= 1u128 << sign_bit;
        }
        let lit = self.scalar(bits, ty);
        let bool_ty = self.bool_ty(At::Pat(p))?;
        let eq = self.scratch(e, bool_ty);
        self.b.assign(eq, Value::Binary { op: BBin::Eq, lhs: Operand::Copy(place), rhs: lit }, span);
        let ok = self.b.new_block(BlockKind::Normal);
        let cases = self.b.cases(&[(0, fail)]);
        self.b.terminate(StmtKind::Switch { on: Operand::Move(eq), cases, otherwise: ok }, span);
        self.b.start(ok);
        Ok(())
    }

    /// Tests a string or byte-string literal pattern (`"..."` or `b"..."`) against `place`
    /// (`&str`, `&[u8]`, `&[u8; N]`, or `[u8; N]`): checks length when dynamically sized, then
    /// compares each byte by `Proj::ConstIndex`.
    fn test_str_pat(
        &mut self,
        p: PatId,
        byte_str: bool,
        place: Place,
        ty: PTy,
        fail: Block,
        e: ExprId,
        span: Span,
    ) -> Result<(), LowerError> {
        let Some(bytes) = self.facts.str_pat(p) else {
            return self.refuse_pat(Construct::ValuePattern, p);
        };
        let mut owned_bytes: Vec<u8, A> = Vec::with_capacity_in(bytes.len(), self.alloc);
        owned_bytes.extend_from_slice(bytes);
        let (seq_place, wide) = match self.node(ty, At::Pat(p))? {
            PNode::Ref(_, inner) => match self.node(inner, At::Pat(p))? {
                PNode::Str if !byte_str => (self.b.project(place, Proj::Deref), true),
                PNode::Slice(elem) if byte_str && self.node(elem, At::Pat(p))? == PNode::Uint(UintTy::U8) => {
                    (self.b.project(place, Proj::Deref), true)
                }
                PNode::Array(elem, _) if byte_str && self.node(elem, At::Pat(p))? == PNode::Uint(UintTy::U8) => {
                    (self.b.project(place, Proj::Deref), false)
                }
                _ => return self.refuse_pat(Construct::ValuePattern, p),
            },
            PNode::Array(elem, _) if byte_str && self.node(elem, At::Pat(p))? == PNode::Uint(UintTy::U8) => {
                (place, false)
            }
            _ => return self.refuse_pat(Construct::ValuePattern, p),
        };
        let Ok(len_u32) = u32::try_from(owned_bytes.len()) else {
            return self.refuse_pat(Construct::ValuePattern, p);
        };
        if wide {
            let usize_ty = self.find_ty(PNode::Uint(UintTy::Usize), At::Pat(p))?;
            let len_p = self.scratch(e, usize_ty);
            self.b.assign(len_p, Value::Len(seq_place), span);
            self.switch_eq(Operand::Move(len_p), u128::from(len_u32), fail, span);
        }
        for (i, &byte) in owned_bytes.iter().enumerate() {
            let Ok(offset) = u32::try_from(i) else {
                return self.refuse_pat(Construct::ValuePattern, p);
            };
            let bp = self.b.project(seq_place, Proj::ConstIndex { offset, min_length: len_u32, from_end: false });
            self.switch_eq(Operand::Copy(bp), u128::from(byte), fail, span);
        }
        Ok(())
    }

    /// Tests a scalar `const` item path pattern (`const K: i32 = ...;`) against `place`.
    fn test_const_pat(
        &mut self,
        p: PatId,
        key: ItemKey,
        place: Place,
        ty: PTy,
        fail: Block,
        e: ExprId,
        span: Span,
    ) -> Result<(), LowerError> {
        let scalar = matches!(
            self.node(ty, At::Pat(p))?,
            PNode::Int(_) | PNode::Uint(_) | PNode::Bool | PNode::Char | PNode::Float(_)
        );
        if !scalar {
            return self.refuse_pat(Construct::ValuePattern, p);
        }
        let args = self.ck.pat_const_args_of(p).unwrap_or(EMPTY_LIST);
        let c = self.b.new_const(ConstDecl { kind: ConstKind::Item { key, args }, ty });
        let bool_ty = self.bool_ty(At::Pat(p))?;
        let eq = self.scratch(e, bool_ty);
        self.b.assign(eq, Value::Binary { op: BBin::Eq, lhs: Operand::Copy(place), rhs: Operand::Const(c) }, span);
        let ok = self.b.new_block(BlockKind::Normal);
        let cases = self.b.cases(&[(0, fail)]);
        self.b.terminate(StmtKind::Switch { on: Operand::Move(eq), cases, otherwise: ok }, span);
        self.b.start(ok);
        Ok(())
    }

    /// Whether pattern `root` introduces any `Pat::Bind`.
    fn pat_has_bindings(&self, root: PatId) -> Result<bool, LowerError> {
        let mut work: Vec<PatId, A> = Vec::new_in(self.alloc);
        work.push(root);
        let mut steps = 0u32;
        while let Some(q) = work.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse_pat(Construct::Bound, root);
            }
            match self.pat(q)? {
                Pat::Bind { .. } => return Ok(true),
                Pat::Ref { inner, .. } => work.push(inner),
                Pat::Tuple { elems, .. } | Pat::TupleStruct { elems, .. } | Pat::Slice { elems, .. } => {
                    work.extend_from_slice(self.pats_in(elems, q)?);
                }
                Pat::Or { alts } => work.extend_from_slice(self.pats_in(alts, q)?),
                Pat::Struct { fields, .. } | Pat::Record { fields, .. } => {
                    for fp in self.field_pats_in(fields, q)? {
                        work.push(fp.pat);
                    }
                }
                Pat::Missing | Pat::Wild | Pat::Lit(_) | Pat::NegLit(_) | Pat::Path(_) | Pat::Range { .. } => {}
            }
        }
        Ok(false)
    }

    /// Reads one endpoint of `Pat::Range` as an operand: an integer/char literal or a scalar
    /// `const` item path.
    fn range_bound_op(&mut self, bound: PatId, ty: PTy) -> Result<Operand, LowerError> {
        match self.pat(bound)? {
            Pat::Lit(Lit::Int(_) | Lit::Char) | Pat::NegLit(Lit::Int(_)) => match self.facts.int_pat(bound) {
                Some(v) => Ok(self.scalar(v, ty)),
                None => self.refuse_pat(Construct::NoLiteralValue, bound),
            },
            Pat::Path(_) => match self.pat_def(bound) {
                Some(Def::Const(key)) => {
                    let args = self.ck.pat_const_args_of(bound).unwrap_or(EMPTY_LIST);
                    let c = self.b.new_const(ConstDecl { kind: ConstKind::Item { key, args }, ty });
                    Ok(Operand::Const(c))
                }
                _ => self.refuse_pat(Construct::ValuePattern, bound),
            },
            _ => self.refuse_pat(Construct::ValuePattern, bound),
        }
    }

    /// Tests `lo..=hi` / `lo..hi` / `lo..` / `..=hi` against `place` (of integer/char type `ty`).
    fn test_range_pat(
        &mut self,
        p: PatId,
        lo: Option<PatId>,
        hi: Option<PatId>,
        inclusive: bool,
        place: Place,
        ty: PTy,
        fail: Block,
        e: ExprId,
    ) -> Result<(), LowerError> {
        let span = self.pat_span(p);
        let bool_ty = self.bool_ty(At::Pat(p))?;
        if let Some(lp) = lo {
            let lo_c = self.range_bound_op(lp, ty)?;
            let cmp = self.scratch(e, bool_ty);
            self.b.assign(cmp, Value::Binary { op: BBin::Ge, lhs: Operand::Copy(place), rhs: lo_c }, span);
            let ok = self.b.new_block(BlockKind::Normal);
            let cases = self.b.cases(&[(0, fail)]);
            self.b.terminate(StmtKind::Switch { on: Operand::Move(cmp), cases, otherwise: ok }, span);
            self.b.start(ok);
        }
        if let Some(hp) = hi {
            let hi_c = self.range_bound_op(hp, ty)?;
            let op = if inclusive { BBin::Le } else { BBin::Lt };
            let cmp = self.scratch(e, bool_ty);
            self.b.assign(cmp, Value::Binary { op, lhs: Operand::Copy(place), rhs: hi_c }, span);
            let ok = self.b.new_block(BlockKind::Normal);
            let cases = self.b.cases(&[(0, fail)]);
            self.b.terminate(StmtKind::Switch { on: Operand::Move(cmp), cases, otherwise: ok }, span);
            self.b.start(ok);
        }
        Ok(())
    }

    /// Element projections for a slice or array pattern `[p0, .., pk]`: returns `(proj, sub_pat, sub_ty)`
    /// for each element and optional rest subslice.
    fn slice_elems(
        &self,
        p: PatId,
        elems: List,
        rest: Option<u32>,
        ty: PTy,
    ) -> Result<(bool, u32, Vec<(Proj, PatId, PTy), A>), LowerError> {
        let (is_slice, elem_ty) = match self.node(ty, At::Pat(p))? {
            PNode::Slice(t) => (true, t),
            PNode::Array(t, _) => (false, t),
            _ => return self.refuse_pat(Construct::Malformed, p),
        };
        let subs = self.pats_in(elems, p)?;
        let Ok(len) = u32::try_from(subs.len()) else { return self.refuse_pat(Construct::Malformed, p) };
        let min_length = if rest.is_some() { len.saturating_sub(1) } else { len };
        let mut out: Vec<(Proj, PatId, PTy), A> = Vec::with_capacity_in(subs.len(), self.alloc);
        for (idx, &sub) in subs.iter().enumerate() {
            let Ok(i) = u32::try_from(idx) else { return self.refuse_pat(Construct::Malformed, sub) };
            match rest {
                Some(r) if i == r => {
                    let sub_ty = self.pat_ty(sub)?;
                    let proj = Proj::Subslice { from: r, to: len - 1 - r, from_end: true };
                    out.push((proj, sub, sub_ty));
                }
                Some(r) if i > r => {
                    let proj = Proj::ConstIndex { offset: len - i, min_length, from_end: true };
                    out.push((proj, sub, elem_ty));
                }
                _ => {
                    let proj = Proj::ConstIndex { offset: i, min_length, from_end: false };
                    out.push((proj, sub, elem_ty));
                }
            }
        }
        Ok((is_slice, min_length, out))
    }

    /// Tests a slice/array pattern: checks length for slices, then tests each element/subslice.
    fn test_slice_pat(
        &mut self,
        p: PatId,
        elems: List,
        rest: Option<u32>,
        place: Place,
        ty: PTy,
        fail: Block,
        e: ExprId,
        choice: Option<&[OrChoice]>,
    ) -> Result<(), LowerError> {
        let span = self.pat_span(p);
        let (is_slice, min_length, parts) = self.slice_elems(p, elems, rest, ty)?;
        if is_slice {
            let usize_ty = self.find_ty(PNode::Uint(UintTy::Usize), At::Pat(p))?;
            let len_p = self.scratch(e, usize_ty);
            self.b.assign(len_p, Value::Len(place), span);
            if rest.is_some() {
                if min_length > 0 {
                    let bool_ty = self.bool_ty(At::Pat(p))?;
                    let min_c = self.scalar(u128::from(min_length), usize_ty);
                    let ok_flag = self.scratch(e, bool_ty);
                    self.b.assign(ok_flag, Value::Binary { op: BBin::Ge, lhs: Operand::Move(len_p), rhs: min_c }, span);
                    let ok = self.b.new_block(BlockKind::Normal);
                    let cases = self.b.cases(&[(0, fail)]);
                    self.b.terminate(StmtKind::Switch { on: Operand::Move(ok_flag), cases, otherwise: ok }, span);
                    self.b.start(ok);
                }
            } else {
                self.switch_eq(Operand::Move(len_p), u128::from(min_length), fail, span);
            }
        }
        for &(proj, sub, sub_ty) in parts.iter() {
            if matches!(self.pat(sub)?, Pat::Wild) {
                continue;
            }
            let sp = self.b.project(place, proj);
            self.test_pat_opt(sub, sp, sub_ty, fail, e, choice)?;
        }
        Ok(())
    }

    /// `switch on [bits -> ok] else fail`, then `ok` open.
    fn switch_eq(&mut self, on: Operand, bits: u128, fail: Block, span: Span) {
        let ok = self.b.new_block(BlockKind::Normal);
        let cases = self.b.cases(&[(bits, ok)]);
        self.b.terminate(StmtKind::Switch { on, cases, otherwise: fail }, span);
        self.b.start(ok);
    }

    /// The test that `place` holds variant `variant` of `adt`: none for a struct; for an enum,
    /// `switch` on its discriminant.
    fn variant_test(&mut self, adt: ItemKey, variant: u32, place: Place, fail: Block, p: PatId, e: ExprId) -> Result<(), LowerError> {
        if !self.adt_facts(adt, At::Pat(p))?.is_enum {
            return Ok(());
        }
        let Some(d) = self.facts.discr(adt, variant) else { return self.refuse_pat(Construct::Discriminant, p) };
        let n = match d.int {
            DiscrInt::Int(i) => PNode::Int(i),
            DiscrInt::Uint(u) => PNode::Uint(u),
        };
        let dty = self.find_ty(n, At::Pat(p))?;
        let t = self.scratch(e, dty);
        let span = self.pat_span(p);
        self.b.assign(t, Value::Discriminant(place), span);
        self.switch_eq(Operand::Move(t), d.bits, fail, span);
        Ok(())
    }

    /// Whether `p` matches every value of its type (no literal, range, slice, or enum variant
    /// test).
    fn irrefutable(&self, p: PatId) -> Result<bool, LowerError> {
        let mut work: Vec<PatId, A> = Vec::new_in(self.alloc);
        work.push(p);
        let mut steps = 0u32;
        while let Some(q) = work.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse_pat(Construct::Bound, p);
            }
            match self.pat(q)? {
                Pat::Missing => return self.refuse_pat(Construct::Malformed, q),
                Pat::Wild => {}
                Pat::Bind { sub, .. } => {
                    if self.ck.union_pat_of(q).is_some() {
                        return Ok(false);
                    }
                    if let Some(s) = sub {
                        work.push(s);
                    }
                }
                Pat::Ref { inner, .. } => work.push(inner),
                Pat::Tuple { elems, .. } => work.extend_from_slice(self.pats_in(elems, q)?),
                Pat::TupleStruct { elems, .. } => {
                    let (adt, _) = self.ctor_of(q)?;
                    if self.adt_facts(adt, At::Pat(q))?.is_enum {
                        return Ok(false);
                    }
                    work.extend_from_slice(self.pats_in(elems, q)?);
                }
                Pat::Struct { fields, .. } => {
                    let (adt, _) = self.ctor_of(q)?;
                    if self.adt_facts(adt, At::Pat(q))?.is_enum {
                        return Ok(false);
                    }
                    for fp in self.field_pats_in(fields, q)? {
                        work.push(fp.pat);
                    }
                }
                Pat::Path(_) => {
                    let (adt, _) = self.ctor_of(q)?;
                    if self.adt_facts(adt, At::Pat(q))?.is_enum {
                        return Ok(false);
                    }
                }
                Pat::Record { fields, .. } => {
                    for fp in self.field_pats_in(fields, q)? {
                        work.push(fp.pat);
                    }
                }
                Pat::Slice { elems, rest } => {
                    let pt = self.pat_ty(q)?;
                    match self.node(pt, At::Pat(q))? {
                        PNode::Array(..) => work.extend_from_slice(self.pats_in(elems, q)?),
                        PNode::Slice(_) => {
                            let subs = self.pats_in(elems, q)?;
                            if subs.len() == 1 && rest == Some(0) {
                                work.push(subs[0]);
                            } else {
                                return Ok(false);
                            }
                        }
                        _ => return Ok(false),
                    }
                }
                Pat::Lit(_) | Pat::NegLit(_) | Pat::Range { .. } | Pat::Or { .. } => return Ok(false),
            }
        }
        Ok(true)
    }

    /// The bindings of irrefutable pattern `p` (a `let`, a parameter) from `place`.
    pub(crate) fn bind_irrefutable(&mut self, p: PatId, place: Place, ty: PTy, out: &mut Vec<Binding, A>) -> Result<(), LowerError> {
        if !self.irrefutable(p)? {
            return self.refuse_pat(Construct::Refutable, p);
        }
        self.bind_pat(p, place, ty, out)
    }

    /// The bindings of pattern `p` from `place` (of type `ty`), already tested: each binding's
    /// storage begun and its value moved, copied or borrowed in, in the order the pattern
    /// writes them; `out` gets them to schedule.
    pub(crate) fn bind_pat(&mut self, p: PatId, place: Place, ty: PTy, out: &mut Vec<Binding, A>) -> Result<(), LowerError> {
        self.bind_pat_opt(p, place, ty, out, None, None)
    }

    fn bind_pat_opt(
        &mut self,
        p: PatId,
        place: Place,
        ty: PTy,
        out: &mut Vec<Binding, A>,
        choice: Option<&[OrChoice]>,
        existing: Option<&[(Name, Place)]>,
    ) -> Result<(), LowerError> {
        self.depth += 1;
        if self.depth > crate::MOST_DEPTH {
            return self.refuse_pat(Construct::Depth, p);
        }
        let r = self.bind_inner(p, place, ty, out, choice, existing);
        self.depth -= 1;
        r
    }

    fn bind_inner(
        &mut self,
        p: PatId,
        place: Place,
        ty: PTy,
        out: &mut Vec<Binding, A>,
        choice: Option<&[OrChoice]>,
        existing: Option<&[(Name, Place)]>,
    ) -> Result<(), LowerError> {
        let (place, ty) = self.peel(p, place, ty)?;
        let span = self.pat_span(p);
        match self.pat(p)? {
            Pat::Missing => self.refuse_pat(Construct::Malformed, p),
            Pat::Wild | Pat::Lit(_) | Pat::NegLit(_) | Pat::Path(_) | Pat::Range { .. } => Ok(()),
            Pat::Bind { ann, name, sub, .. } => {
                let lt = self.local_ty(p)?;
                match self.ck.union_pat_of(p) {
                    None => {
                        let by_ref = match ann.by_ref {
                            Some(m) => Some(m),
                            None if lt != ty => match self.node(lt, At::Pat(p))? {
                                PNode::Ref(m, inner) if inner == ty => Some(m),
                                _ => return self.refuse_pat(Construct::Malformed, p),
                            },
                            None => None,
                        };
                        self.bind_value(p, name, place, lt, by_ref, span, out, existing)?;
                        match sub {
                            Some(s) => self.bind_pat_opt(s, place, ty, out, choice, existing),
                            None => Ok(()),
                        }
                    }
                    Some(UnionPat::Variant(v)) => {
                        let PNode::Union(ul) = self.node(ty, At::Pat(p))? else {
                            return self.refuse_pat(Construct::Malformed, p);
                        };
                        let Ok(v_idx) = usize::try_from(v) else { return self.refuse_pat(Construct::Malformed, p) };
                        let Some(&member_ty) = self.list(ul, At::Pat(p))?.get(v_idx) else {
                            return self.refuse_pat(Construct::Malformed, p);
                        };
                        let dp = self.b.project(place, Proj::Downcast(v));
                        let fp = self.b.project(dp, Proj::Field { index: 0, ty: member_ty });
                        let by_ref = match ann.by_ref {
                            Some(m) => Some(m),
                            None if lt != member_ty => match self.node(lt, At::Pat(p))? {
                                PNode::Ref(m, inner) if inner == member_ty => Some(m),
                                _ => return self.refuse_pat(Construct::Malformed, p),
                            },
                            None => None,
                        };
                        self.bind_value(p, name, fp, lt, by_ref, span, out, existing)?;
                        match sub {
                            Some(s) => self.bind_pat_opt(s, fp, member_ty, out, choice, existing),
                            None => Ok(()),
                        }
                    }
                    Some(UnionPat::Residual) => {
                        let (by_ref, dst_ty) = match ann.by_ref {
                            Some(m) => match self.node(lt, At::Pat(p))? {
                                PNode::Ref(_, inner) if matches!(self.node(inner, At::Pat(p))?, PNode::Union(_)) => (Some(m), inner),
                                _ => return self.refuse_pat(Construct::Malformed, p),
                            },
                            None => match self.node(lt, At::Pat(p))? {
                                PNode::Ref(m, inner) if matches!(self.node(inner, At::Pat(p))?, PNode::Union(_)) => (Some(m), inner),
                                PNode::Union(_) => (None, lt),
                                _ => return self.refuse_pat(Construct::Malformed, p),
                            },
                        };
                        let root = self.body.root;
                        let narrowed_place = match by_ref {
                            Some(m) => {
                                let tp = self.temp(root, dst_ty, Role::Value)?;
                                self.remap_union(place, ty, dst_ty, tp, root, span, At::Pat(p))?;
                                self.bind_value(p, name, tp, lt, Some(m), span, out, existing)?;
                                tp
                            }
                            None => match existing.and_then(|map| map.iter().find(|&&(n, _)| n == name).map(|&(_, lp)| lp)) {
                                Some(lp) => {
                                    self.remap_union(place, ty, dst_ty, lp, root, span, At::Pat(p))?;
                                    self.bind(p, lp);
                                    lp
                                }
                                None => {
                                    let (local, lp) = self.binding_local(p, lt)?;
                                    self.remap_union(place, ty, dst_ty, lp, root, span, At::Pat(p))?;
                                    let drops = self.needs_drop(lt, At::Pat(p))?;
                                    out.push(Binding { local, place: lp, ty: lt, drops });
                                    self.bind(p, lp);
                                    lp
                                }
                            },
                        };
                        match sub {
                            Some(s) => self.bind_pat_opt(s, narrowed_place, dst_ty, out, choice, existing),
                            None => Ok(()),
                        }
                    }
                }
            }
            Pat::Ref { inner, .. } => {
                let it = self.pointee(ty, At::Pat(p))?;
                let ip = self.b.project(place, Proj::Deref);
                self.bind_pat_opt(inner, ip, it, out, choice, existing)
            }
            Pat::Tuple { .. } | Pat::TupleStruct { .. } | Pat::Struct { .. } | Pat::Record { .. } => {
                let (base, marked, subs) = self.sub_fields(p, place, ty)?;
                for &(index, s, fty) in subs.iter() {
                    let fp = self.b.project_marked(base, Proj::Field { index, ty: fty }, marked);
                    self.bind_pat_opt(s, fp, fty, out, choice, existing)?;
                }
                Ok(())
            }
            Pat::Or { alts } => {
                if let Some(ch) = choice {
                    let list = self.pats_in(alts, p)?;
                    let Some(&(_, alt_i, _)) = ch.iter().find(|&&(q, _, _)| q == p) else {
                        return self.refuse_pat(Construct::Malformed, p);
                    };
                    let Ok(ix) = usize::try_from(alt_i) else {
                        return self.refuse_pat(Construct::Malformed, p);
                    };
                    let Some(&alt) = list.get(ix) else {
                        return self.refuse_pat(Construct::Malformed, p);
                    };
                    return self.bind_pat_opt(alt, place, ty, out, Some(ch), existing);
                }
                if self.pat_has_bindings(p)? {
                    return self.refuse_pat(Construct::OrPattern, p);
                }
                Ok(())
            }
            Pat::Slice { elems, rest } => {
                let (_, _, parts) = self.slice_elems(p, elems, rest, ty)?;
                for &(proj, sub, sub_ty) in parts.iter() {
                    if matches!(self.pat(sub)?, Pat::Wild) {
                        continue;
                    }
                    let sp = self.b.project(place, proj);
                    self.bind_pat_opt(sub, sp, sub_ty, out, choice, existing)?;
                }
                Ok(())
            }
        }
    }

    /// Creates binding `p`'s local of type `lt` (or reuses its pre-allocated or-pattern local
    /// from `existing`) and assigns or borrows `source` into it.
    fn bind_value(
        &mut self,
        p: PatId,
        name: Name,
        source: Place,
        lt: PTy,
        by_ref: Option<Mutbl>,
        span: Span,
        out: &mut Vec<Binding, A>,
        existing: Option<&[(Name, Place)]>,
    ) -> Result<(), LowerError> {
        let lp = match existing.and_then(|map| map.iter().find(|&&(n, _)| n == name).map(|&(_, lp)| lp)) {
            Some(lp) => lp,
            None => {
                let (local, lp) = self.binding_local(p, lt)?;
                let drops = self.needs_drop(lt, At::Pat(p))?;
                out.push(Binding { local, place: lp, ty: lt, drops });
                lp
            }
        };
        match by_ref {
            Some(m) => {
                let kind = match m {
                    Mutbl::Not => BorrowKind::Shared,
                    Mutbl::Mut => BorrowKind::Mut,
                };
                let o = self.b.origin(false);
                self.b.borrow(lp, kind, source, o, span);
            }
            None => {
                let op = if self.is_copy(lt, At::Pat(p))? { Operand::Copy(source) } else { Operand::Move(source) };
                self.b.assign(lp, Value::Use(op), span);
            }
        }
        self.bind(p, lp);
        Ok(())
    }

    /// `let p;`: each binding's storage begun, nothing written.
    pub(crate) fn declare_uninit(&mut self, p: PatId, out: &mut Vec<Binding, A>) -> Result<(), LowerError> {
        let mut work: Vec<PatId, A> = Vec::new_in(self.alloc);
        work.push(p);
        let mut steps = 0u32;
        while let Some(cur) = work.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse_pat(Construct::Bound, cur);
            }
            match self.pat(cur)? {
                Pat::Wild => {}
                Pat::Bind { sub: None, .. } => {
                    let lt = self.local_ty(cur)?;
                    let (local, lp) = self.binding_local(cur, lt)?;
                    let drops = self.needs_drop(lt, At::Pat(cur))?;
                    out.push(Binding { local, place: lp, ty: lt, drops });
                    self.bind(cur, lp);
                }
                Pat::Tuple { elems, .. } => {
                    for &s in self.pats_in(elems, cur)?.iter().rev() {
                        work.push(s);
                    }
                }
                Pat::Record { fields, .. } => {
                    for fp in self.field_pats_in(fields, cur)?.iter().rev() {
                        work.push(fp.pat);
                    }
                }
                _ => return self.refuse_pat(Construct::Refutable, cur),
            }
        }
        Ok(())
    }

    /// Whether `root` contains a `Pat::Or` that introduces at least one binding.
    fn pat_has_binding_or(&self, root: PatId) -> Result<bool, LowerError> {
        let mut work: Vec<PatId, A> = Vec::new_in(self.alloc);
        work.push(root);
        let mut steps = 0u32;
        while let Some(q) = work.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse_pat(Construct::Bound, root);
            }
            match self.pat(q)? {
                Pat::Or { .. } if self.pat_has_bindings(q)? => return Ok(true),
                Pat::Bind { sub: Some(s), .. } => work.push(s),
                Pat::Ref { inner, .. } => work.push(inner),
                Pat::Tuple { elems, .. } | Pat::TupleStruct { elems, .. } | Pat::Slice { elems, .. } => {
                    work.extend_from_slice(self.pats_in(elems, q)?);
                }
                Pat::Struct { fields, .. } | Pat::Record { fields, .. } => {
                    for fp in self.field_pats_in(fields, q)? {
                        work.push(fp.pat);
                    }
                }
                _ => {}
            }
        }
        Ok(false)
    }

    /// Each distinct binding name in `root`, paired with its first `Pat::Bind` in source order
    /// (pre-order, left to right): the one local every or-alternative stores into.
    fn first_binds(&self, root: PatId) -> Result<Vec<(Name, PatId), A>, LowerError> {
        let mut out: Vec<(Name, PatId), A> = Vec::new_in(self.alloc);
        let mut work: Vec<PatId, A> = Vec::new_in(self.alloc);
        work.push(root);
        let mut steps = 0u32;
        while let Some(q) = work.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse_pat(Construct::Bound, root);
            }
            match self.pat(q)? {
                Pat::Bind { name, sub, .. } => {
                    if !out.iter().any(|&(n, _)| n == name) {
                        out.push((name, q));
                    }
                    if let Some(s) = sub {
                        work.push(s);
                    }
                }
                Pat::Ref { inner, .. } => work.push(inner),
                Pat::Tuple { elems, .. }
                | Pat::TupleStruct { elems, .. }
                | Pat::Slice { elems, .. }
                | Pat::Or { alts: elems } => {
                    for &s in self.pats_in(elems, q)?.iter().rev() {
                        work.push(s);
                    }
                }
                Pat::Struct { fields, .. } | Pat::Record { fields, .. } => {
                    for fp in self.field_pats_in(fields, q)?.iter().rev() {
                        work.push(fp.pat);
                    }
                }
                Pat::Missing | Pat::Wild | Pat::Lit(_) | Pat::NegLit(_) | Pat::Path(_) | Pat::Range { .. } => {}
            }
        }
        Ok(out)
    }

    /// Completes `choice` with the or-patterns met in `root` in pre-order (entering only the
    /// chosen alternative of each `Pat::Or`).
    fn choose_or(&self, root: PatId, choice: &mut Vec<OrChoice, A>) -> Result<(), LowerError> {
        let kept = choice.len();
        let mut met = 0usize;
        let mut work: Vec<PatId, A> = Vec::new_in(self.alloc);
        work.push(root);
        let mut steps = 0u32;
        while let Some(q) = work.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse_pat(Construct::Bound, root);
            }
            match self.pat(q)? {
                Pat::Or { alts } => {
                    let list = self.pats_in(alts, q)?;
                    let Ok(n) = u32::try_from(list.len()) else { return self.refuse_pat(Construct::Malformed, q) };
                    let j = if met < kept {
                        let Some(&(was, j, _)) = choice.get(met) else {
                            return self.refuse_pat(Construct::Malformed, q);
                        };
                        if was != q {
                            return self.refuse_pat(Construct::Malformed, q);
                        }
                        j
                    } else {
                        choice.push((q, 0, n));
                        0
                    };
                    met += 1;
                    let Ok(ix) = usize::try_from(j) else { return self.refuse_pat(Construct::Malformed, q) };
                    let Some(&alt) = list.get(ix) else { return self.refuse_pat(Construct::Malformed, q) };
                    work.push(alt);
                }
                Pat::Bind { sub: Some(s), .. } => work.push(s),
                Pat::Ref { inner, .. } => work.push(inner),
                Pat::Tuple { elems, .. } | Pat::TupleStruct { elems, .. } | Pat::Slice { elems, .. } => {
                    for &s in self.pats_in(elems, q)?.iter().rev() {
                        work.push(s);
                    }
                }
                Pat::Struct { fields, .. } | Pat::Record { fields, .. } => {
                    for fp in self.field_pats_in(fields, q)?.iter().rev() {
                        work.push(fp.pat);
                    }
                }
                _ => {}
            }
        }
        if met < kept {
            return self.refuse_pat(Construct::Malformed, root);
        }
        Ok(())
    }

    /// Advances `choice` to the next or-free expansion; returns `false` after the last one.
    fn advance_or(choice: &mut Vec<OrChoice, A>) -> bool {
        while let Some((p, j, n)) = choice.pop() {
            if j + 1 < n {
                choice.push((p, j + 1, n));
                return true;
            }
        }
        false
    }

    /// Tests pattern `pat` against `place` (of type `ty`) and binds its variables into `out`.
    /// On failure, jumps to `fail` with no bindings left live; on success, leaves the matched
    /// block open with all bindings written and recorded in `out`. Supports binding `OrPattern`
    /// (`Some(x) | Other(x)`) by expanding into or-free alternatives so all tests of an
    /// alternative pass before any binding is written or moved.
    pub(crate) fn match_and_bind(
        &mut self,
        pat: PatId,
        place: Place,
        ty: PTy,
        fail: Block,
        e: ExprId,
        out: &mut Vec<Binding, A>,
    ) -> Result<(), LowerError> {
        if !self.pat_has_binding_or(pat)? {
            self.test_pat(pat, place, ty, fail, e)?;
            return self.bind_pat(pat, place, ty, out);
        }
        let span = self.pat_span(pat);
        let firsts = self.first_binds(pat)?;
        let mut map: Vec<(Name, Place), A> = Vec::with_capacity_in(firsts.len(), self.alloc);
        for &(name, first_pat) in firsts.iter() {
            let lt = self.local_ty(first_pat)?;
            let (local, lp) = self.binding_local(first_pat, lt)?;
            let drops = self.needs_drop(lt, At::Pat(first_pat))?;
            out.push(Binding { local, place: lp, ty: lt, drops });
            self.bind(first_pat, lp);
            map.push((name, lp));
        }
        let matched = self.b.new_block(BlockKind::Normal);
        let all_fail = self.b.new_block(BlockKind::Normal);
        let mut choice: Vec<OrChoice, A> = Vec::new_in(self.alloc);
        let mut n = 0u32;
        loop {
            self.choose_or(pat, &mut choice)?;
            if n == MOST_OR_ALTS {
                return self.refuse_pat(Construct::OrPattern, pat);
            }
            let next_alt = self.b.new_block(BlockKind::Normal);
            self.test_pat_opt(pat, place, ty, next_alt, e, Some(choice.as_slice()))?;
            self.bind_pat_opt(pat, place, ty, out, Some(choice.as_slice()), Some(map.as_slice()))?;
            self.goto(matched, span);
            self.b.start(next_alt);
            n += 1;
            if !Self::advance_or(&mut choice) {
                self.goto(all_fail, span);
                break;
            }
        }
        self.b.start(all_fail);
        for b in out.iter().rev() {
            self.b.push(StmtKind::StorageDead(b.local), span);
        }
        self.goto(fail, span);
        self.b.start(matched);
        Ok(())
    }

    /// `match scrutinee { arms }` into `dest`, arm by arm (the module's rule). An arm's guard
    /// runs with its bindings made (each must be `Copy`: a guard over a by-move binding is
    /// refused) and the scrutinee shallowly borrowed, so the guard cannot change what matched.
    pub(crate) fn match_into(&mut self, dest: Place, e: ExprId, scrutinee: ExprId, arms: List) -> Result<(), LowerError> {
        let span = self.span(e);
        // The Reference, [destructors.scope.temporary]: the scrutinee of a `match` expression is
        // NOT a temporary scope, so temporaries in the scrutinee stay alive until the enclosing
        // statement's temporary scope ends.
        let sty = self.adjusted_ty(scrutinee)?;
        let sp = self.place(scrutinee)?;
        self.b.push(StmtKind::FakeRead(sp), span);
        let join = self.b.new_block(BlockKind::Normal);
        for &arm in self.arms_in(arms, e)? {
            let fail = self.b.new_block(BlockKind::Normal);
            let bm = self.open();
            let mut out: Vec<Binding, A> = Vec::new_in(self.alloc);
            let guard_or_resume: Option<(Place, Vec<Block, A>)> = if arm.guard.is_some() && self.pat_has_binding_or(arm.pat)? {
                let firsts = self.first_binds(arm.pat)?;
                let mut map: Vec<(Name, Place), A> = Vec::with_capacity_in(firsts.len(), self.alloc);
                for &(name, first_pat) in firsts.iter() {
                    let lt = self.local_ty(first_pat)?;
                    let (local, lp) = self.binding_local(first_pat, lt)?;
                    let drops = self.needs_drop(lt, At::Pat(first_pat))?;
                    out.push(Binding { local, place: lp, ty: lt, drops });
                    self.bind(first_pat, lp);
                    map.push((name, lp));
                }
                let u32_ty = self.find_ty(PNode::Uint(UintTy::U32), At::Pat(arm.pat))?;
                let idx_place = self.scratch(e, u32_ty);
                let matched = self.b.new_block(BlockKind::Normal);
                let all_fail = self.b.new_block(BlockKind::Normal);
                let mut resume_blocks: Vec<Block, A> = Vec::new_in(self.alloc);
                let mut choice: Vec<OrChoice, A> = Vec::new_in(self.alloc);
                let mut n = 0u32;
                loop {
                    self.choose_or(arm.pat, &mut choice)?;
                    if n == MOST_OR_ALTS {
                        return self.refuse_pat(Construct::OrPattern, arm.pat);
                    }
                    let next_alt = self.b.new_block(BlockKind::Normal);
                    resume_blocks.push(next_alt);
                    self.test_pat_opt(arm.pat, sp, sty, next_alt, e, Some(choice.as_slice()))?;
                    self.bind_pat_opt(arm.pat, sp, sty, &mut out, Some(choice.as_slice()), Some(map.as_slice()))?;
                    let k = self.scalar(u128::from(n), u32_ty);
                    self.b.assign(idx_place, Value::Use(k), span);
                    self.goto(matched, span);
                    self.b.start(next_alt);
                    n += 1;
                    if !Self::advance_or(&mut choice) {
                        self.goto(all_fail, span);
                        break;
                    }
                }
                self.b.start(all_fail);
                for b in out.iter().rev() {
                    self.b.push(StmtKind::StorageDead(b.local), span);
                }
                self.goto(fail, span);
                self.b.start(matched);
                Some((idx_place, resume_blocks))
            } else {
                self.match_and_bind(arm.pat, sp, sty, fail, e, &mut out)?;
                None
            };
            self.schedule(&out);
            let mut guard_let_open = false;
            if let Some(g) = arm.guard {
                for x in out.iter() {
                    if !self.is_copy(x.ty, At::Pat(arm.pat))? {
                        return self.refuse_pat(Construct::GuardBindsByMove, arm.pat);
                    }
                }
                let rty = self.find_ty(PNode::Ref(Mutbl::Not, sty), At::Expr(g))?;
                let fb = self.scratch(g, rty);
                let o = self.b.origin(false);
                self.b.borrow(fb, BorrowKind::Shallow, sp, o, span);
                let gm = self.open();
                let gfail = self.b.new_block(BlockKind::Normal);
                if self.cond_has_let(g)? {
                    let links = self.links_of(g)?;
                    for &link in links.iter() {
                        match self.expr(link)? {
                            Expr::Let { pat, init } => {
                                let ity = self.adjusted_ty(init)?;
                                let ip = self.place(init)?;
                                let lfail = self.b.new_block(BlockKind::Normal);
                                let lok = self.b.new_block(BlockKind::Normal);
                                let mut gout: Vec<Binding, A> = Vec::new_in(self.alloc);
                                self.match_and_bind(pat, ip, ity, lfail, g, &mut gout)?;
                                self.goto(lok, span);
                                self.b.start(lfail);
                                self.emit_exit(gm, span)?;
                                self.goto(gfail, span);
                                self.b.start(lok);
                                self.schedule(&gout);
                            }
                            _ => {
                                let c = self.operand(link)?;
                                let lok = self.b.new_block(BlockKind::Normal);
                                let lfail = self.b.new_block(BlockKind::Normal);
                                let cases = self.b.cases(&[(0, lfail)]);
                                self.b.terminate(StmtKind::Switch { on: c, cases, otherwise: lok }, span);
                                self.b.start(lfail);
                                self.emit_exit(gm, span)?;
                                self.goto(gfail, span);
                                self.b.start(lok);
                            }
                        }
                    }
                    let gok = self.b.new_block(BlockKind::Normal);
                    self.goto(gok, span);
                    self.b.start(gfail);
                    if let Some((idx_place, ref resume_blocks)) = guard_or_resume {
                        let mut sw: Vec<(u128, Block), A> = Vec::with_capacity_in(resume_blocks.len(), self.alloc);
                        for (j, &blk) in resume_blocks.iter().enumerate() {
                            sw.push((u128::try_from(j).unwrap_or(0), blk));
                        }
                        let cases = self.b.cases(&sw);
                        self.b.terminate(StmtKind::Switch { on: Operand::Copy(idx_place), cases, otherwise: fail }, span);
                    } else {
                        self.emit_exit(bm, span)?;
                        self.goto(fail, span);
                    }
                    self.b.start(gok);
                    self.b.push(StmtKind::FakeRead(fb), span);
                    guard_let_open = true;
                } else {
                    let c = self.operand(g)?;
                    let gok = self.b.new_block(BlockKind::Normal);
                    let cases = self.b.cases(&[(0, gfail)]);
                    self.b.terminate(StmtKind::Switch { on: c, cases, otherwise: gok }, span);
                    self.b.start(gfail);
                    self.emit_exit(gm, span)?;
                    if let Some((idx_place, ref resume_blocks)) = guard_or_resume {
                        let mut sw: Vec<(u128, Block), A> = Vec::with_capacity_in(resume_blocks.len(), self.alloc);
                        for (j, &blk) in resume_blocks.iter().enumerate() {
                            sw.push((u128::try_from(j).unwrap_or(0), blk));
                        }
                        let cases = self.b.cases(&sw);
                        self.b.terminate(StmtKind::Switch { on: Operand::Copy(idx_place), cases, otherwise: fail }, span);
                    } else {
                        self.emit_exit(bm, span)?;
                        self.goto(fail, span);
                    }
                    self.b.start(gok);
                    self.emit_exit(gm, span)?;
                    self.pop_scope();
                    self.b.push(StmtKind::FakeRead(fb), span);
                }
            }
            self.lower_into(dest, arm.body)?;
            if guard_let_open {
                self.close(span)?;
            }
            self.close(span)?;
            self.goto(join, span);
            self.b.start(fail);
        }
        self.b.terminate(StmtKind::Unreachable, span);
        self.b.start(join);
        Ok(())
    }
}
