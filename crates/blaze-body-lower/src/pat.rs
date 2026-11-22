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

use blaze_body::{Block, BlockKind, BorrowKind, LocalDecl, LocalKind, Local, Operand, Place, Proj, Span, StmtKind, Value};
use blaze_own::Vec;
use blaze_typeck::{Def, ExprId, ItemKey, Lit, List, Mutbl, PNode, PTy, Pat, PatId};

use crate::scope::Binding;
use crate::{At, BodyFacts, Construct, DiscrInt, Lower, LowerError, MOST_POSITIONS};

/// Most implicit dereferences one pattern takes.
const MOST_PEELS: u32 = 64;

impl<F: BodyFacts, A: Allocator + Clone> Lower<'_, '_, '_, F, A> {
    /// A binding's local, its storage begun.
    pub(crate) fn binding_local(&mut self, p: PatId, ty: PTy) -> Result<(Local, Place), LowerError> {
        let mutbl = match self.pat(p)? {
            Pat::Bind { ann, .. } if ann.mutable => Mutbl::Mut,
            _ => Mutbl::Not,
        };
        let local = self.b.new_local(LocalDecl { ty, kind: LocalKind::Binding(p), mutbl });
        let place = self.b.local_place(local);
        let span = self.pat_span(p);
        self.b.push(StmtKind::StorageLive(local), span);
        Ok((local, place))
    }

    /// Whether `p` is a pattern that matches a value: not a plain binding, wildcard or
    /// reference pattern, which take their place as it is.
    fn structural(&self, p: PatId) -> Result<bool, LowerError> {
        Ok(!matches!(self.pat(p)?, Pat::Bind { .. } | Pat::Wild | Pat::Missing | Pat::Ref { .. }))
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
        let mut out = Vec::new_in(self.alloc.clone());
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
        let mut out = Vec::new_in(self.alloc.clone());
        match self.pat(p)? {
            Pat::Tuple { elems, rest } => {
                let PNode::Tuple(l) = self.node(ty, At::Pat(p))? else { return self.refuse_pat(Construct::Malformed, p) };
                let tys = self.list(l, At::Pat(p))?;
                for (i, s) in self.positional(elems, rest, tys.len() as u32, p)? {
                    out.push((i, s, tys[i as usize]));
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
        self.depth += 1;
        if self.depth > crate::MOST_DEPTH {
            return self.refuse_pat(Construct::Depth, p);
        }
        let r = self.test_inner(p, place, ty, fail, e);
        self.depth -= 1;
        r
    }

    fn test_inner(&mut self, p: PatId, place: Place, ty: PTy, fail: Block, e: ExprId) -> Result<(), LowerError> {
        let (place, ty) = self.peel(p, place, ty)?;
        let span = self.pat_span(p);
        match self.pat(p)? {
            Pat::Missing => self.refuse_pat(Construct::Malformed, p),
            Pat::Wild => Ok(()),
            Pat::Bind { sub, .. } => match sub {
                Some(s) => self.test_pat(s, place, ty, fail, e),
                None => Ok(()),
            },
            Pat::Ref { inner, .. } => {
                let it = self.pointee(ty, At::Pat(p))?;
                let ip = self.b.project(place, Proj::Deref);
                self.test_pat(inner, ip, it, fail, e)
            }
            Pat::Lit(l) | Pat::NegLit(l) => {
                let bits = match l {
                    Lit::Bool(b) => b as u128,
                    Lit::Int(_) | Lit::Char => match self.facts.int_pat(p) {
                        Some(v) => v,
                        None => return self.refuse_pat(Construct::NoLiteralValue, p),
                    },
                    _ => return self.refuse_pat(Construct::ValuePattern, p),
                };
                self.switch_eq(Operand::Copy(place), bits, fail, span);
                Ok(())
            }
            Pat::Path(_) => match self.pat_def(p) {
                Some(Def::Ctor { adt, variant, .. }) => self.variant_test(adt, variant, place, fail, p, e),
                Some(Def::Const(_)) => self.refuse_pat(Construct::ValuePattern, p),
                _ => self.refuse_pat(Construct::Malformed, p),
            },
            Pat::Tuple { .. } | Pat::TupleStruct { .. } | Pat::Struct { .. } => {
                if let Pat::TupleStruct { .. } | Pat::Struct { .. } = self.pat(p)? {
                    let (adt, variant) = self.ctor_of(p)?;
                    self.variant_test(adt, variant, place, fail, p, e)?;
                }
                let (base, marked, subs) = self.sub_fields(p, place, ty)?;
                for &(index, s, fty) in subs.iter() {
                    let fp = self.b.project_marked(base, Proj::Field { index, ty: fty }, marked);
                    self.test_pat(s, fp, fty, fail, e)?;
                }
                Ok(())
            }
            Pat::Or { .. } => self.refuse_pat(Construct::OrPattern, p),
            Pat::Slice { .. } => self.refuse_pat(Construct::SlicePattern, p),
            Pat::Range { .. } => self.refuse_pat(Construct::RangePattern, p),
        }
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
        let mut work: Vec<PatId, A> = Vec::new_in(self.alloc.clone());
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
                Pat::Lit(_) | Pat::NegLit(_) | Pat::Range { .. } | Pat::Slice { .. } | Pat::Or { .. } => return Ok(false),
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
        self.depth += 1;
        if self.depth > crate::MOST_DEPTH {
            return self.refuse_pat(Construct::Depth, p);
        }
        let r = self.bind_inner(p, place, ty, out);
        self.depth -= 1;
        r
    }

    fn bind_inner(&mut self, p: PatId, place: Place, ty: PTy, out: &mut Vec<Binding, A>) -> Result<(), LowerError> {
        let (place, ty) = self.peel(p, place, ty)?;
        let span = self.pat_span(p);
        match self.pat(p)? {
            Pat::Missing => self.refuse_pat(Construct::Malformed, p),
            Pat::Wild | Pat::Lit(_) | Pat::NegLit(_) | Pat::Path(_) | Pat::Range { .. } => Ok(()),
            Pat::Bind { ann, sub, .. } => {
                let lt = self.local_ty(p)?;
                let by_ref = match ann.by_ref {
                    Some(m) => Some(m),
                    None if lt != ty => match self.node(lt, At::Pat(p))? {
                        PNode::Ref(m, inner) if inner == ty => Some(m),
                        _ => return self.refuse_pat(Construct::Malformed, p),
                    },
                    None => None,
                };
                let (local, lp) = self.binding_local(p, lt)?;
                match by_ref {
                    Some(m) => {
                        let kind = match m {
                            Mutbl::Not => BorrowKind::Shared,
                            Mutbl::Mut => BorrowKind::Mut,
                        };
                        let o = self.b.origin(false);
                        self.b.borrow(lp, kind, place, o, span);
                    }
                    None => {
                        let op = if self.is_copy(lt, At::Pat(p))? { Operand::Copy(place) } else { Operand::Move(place) };
                        self.b.assign(lp, Value::Use(op), span);
                    }
                }
                let drops = self.needs_drop(lt, At::Pat(p))?;
                out.push(Binding { local, place: lp, ty: lt, drops });
                self.bind(p, lp);
                match sub {
                    Some(s) => self.bind_pat(s, place, ty, out),
                    None => Ok(()),
                }
            }
            Pat::Ref { inner, .. } => {
                let it = self.pointee(ty, At::Pat(p))?;
                let ip = self.b.project(place, Proj::Deref);
                self.bind_pat(inner, ip, it, out)
            }
            Pat::Tuple { .. } | Pat::TupleStruct { .. } | Pat::Struct { .. } => {
                let (base, marked, subs) = self.sub_fields(p, place, ty)?;
                for &(index, s, fty) in subs.iter() {
                    let fp = self.b.project_marked(base, Proj::Field { index, ty: fty }, marked);
                    self.bind_pat(s, fp, fty, out)?;
                }
                Ok(())
            }
            Pat::Or { .. } => self.refuse_pat(Construct::OrPattern, p),
            Pat::Slice { .. } => self.refuse_pat(Construct::SlicePattern, p),
        }
    }

    /// `let p;`: each binding's storage begun, nothing written.
    pub(crate) fn declare_uninit(&mut self, p: PatId, out: &mut Vec<Binding, A>) -> Result<(), LowerError> {
        match self.pat(p)? {
            Pat::Wild => Ok(()),
            Pat::Bind { sub: None, .. } => {
                let lt = self.local_ty(p)?;
                let (local, lp) = self.binding_local(p, lt)?;
                let drops = self.needs_drop(lt, At::Pat(p))?;
                out.push(Binding { local, place: lp, ty: lt, drops });
                self.bind(p, lp);
                Ok(())
            }
            Pat::Tuple { elems, .. } => {
                for &s in self.pats_in(elems, p)? {
                    self.declare_uninit(s, out)?;
                }
                Ok(())
            }
            _ => self.refuse_pat(Construct::Refutable, p),
        }
    }

    /// `match scrutinee { arms }` into `dest`, arm by arm (the module's rule). An arm's guard
    /// runs with its bindings made (each must be `Copy`: a guard over a by-move binding is
    /// refused) and the scrutinee shallowly borrowed, so the guard cannot change what matched.
    pub(crate) fn match_into(&mut self, dest: Place, e: ExprId, scrutinee: ExprId, arms: List) -> Result<(), LowerError> {
        let span = self.span(e);
        let tm = self.open();
        let sty = self.adjusted_ty(scrutinee)?;
        let sp = self.place(scrutinee)?;
        self.b.push(StmtKind::FakeRead(sp), span);
        let join = self.b.new_block(BlockKind::Normal);
        for &arm in self.arms_in(arms, e)? {
            let fail = self.b.new_block(BlockKind::Normal);
            self.test_pat(arm.pat, sp, sty, fail, e)?;
            let bm = self.open();
            let mut out: Vec<Binding, A> = Vec::new_in(self.alloc.clone());
            self.bind_pat(arm.pat, sp, sty, &mut out)?;
            self.schedule(&out);
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
                let c = self.operand(g)?;
                let gok = self.b.new_block(BlockKind::Normal);
                let gfail = self.b.new_block(BlockKind::Normal);
                let cases = self.b.cases(&[(0, gfail)]);
                self.b.terminate(StmtKind::Switch { on: c, cases, otherwise: gok }, span);
                self.b.start(gfail);
                self.emit_exit(gm, span)?;
                self.emit_exit(bm, span)?;
                self.goto(fail, span);
                self.b.start(gok);
                self.emit_exit(gm, span)?;
                self.pop_scope();
                self.b.push(StmtKind::FakeRead(fb), span);
            }
            self.lower_into(dest, arm.body)?;
            self.close(span)?;
            self.emit_exit(tm, span)?;
            self.goto(join, span);
            self.b.start(fail);
        }
        self.b.terminate(StmtKind::Unreachable, span);
        self.pop_scope();
        self.b.start(join);
        Ok(())
    }
}
