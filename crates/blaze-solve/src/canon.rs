//! Canonical forms (solve.md, "Responses" and (b)): a goal or a response as `u32` words, with
//! its inference variables replaced by canonical variables numbered by first occurrence
//! (rustc_next_trait_solver canonical/canonicalizer.rs, `Canonicalizer`), and the way back
//! (canonical/mod.rs `instantiate_canonical`, `instantiate_and_apply_query_response`).
//!
//! The words are the record key (rules 21, 25): they hold definitions by their portable keys
//! and terms by structure, never an arena handle or an address. A term is written post-order:
//! its children, then a header word (`tag | arity << 8`), then the kind's payload words. A
//! region other than `'static` is written as `ReErased` (regions are not related: solve.md,
//! "Not ported"), so two goals that differ only in regions share a key.

use blaze_own::{Alloc, Vec};

use crate::assembly::{EnvId, Goal, Via};
use crate::error::{Limit, Malformed, SolveError};
use crate::term::{
    AliasKind, Certainty, Constness, Def, FloatTy, FnPtrHeader, GoalSource, IntTy, List, Mutbl, Polarity, Pred, Rigid, Ty, TyKind,
    TypingMode, UintTy, ValTy, Var, VarKind,
};
use crate::tys::{Relate, Tys};

/// The most words one canonical form holds (rule 18).
pub const MAX_WORDS: usize = 1 << 16;

/// The highest variable index a [`Decode`] accepts (rule 18): its slot table's cap, over every
/// form it decodes until `reset` (a certificate's rows share one numbering).
pub const MAX_VARS: u32 = 1 << 22;

// Term tags.
const T_BOOL: u32 = 1;
const T_CHAR: u32 = 2;
const T_INT: u32 = 3;
const T_UINT: u32 = 4;
const T_FLOAT: u32 = 5;
const T_STR: u32 = 6;
const T_NEVER: u32 = 7;
const T_TUPLE: u32 = 8;
const T_REF: u32 = 9;
const T_PTR: u32 = 10;
const T_ARRAY: u32 = 11;
const T_SLICE: u32 = 12;
const T_ADT: u32 = 13;
const T_FOREIGN: u32 = 14;
const T_FNDEF: u32 = 15;
const T_FNPTR: u32 = 16;
const T_CLOSURE: u32 = 17;
const T_DYN: u32 = 18;
const T_ALIAS: u32 = 19;
const T_PARAM: u32 = 20;
const T_PLACEHOLDER: u32 = 21;
const T_BOUND: u32 = 22;
const T_CANON: u32 = 23;
const T_ERROR: u32 = 24;
const T_RE_STATIC: u32 = 25;
const T_RE_ERASED: u32 = 26;
const T_CONST: u32 = 27;
const T_CONST_PARAM: u32 = 28;
const T_EX_TRAIT: u32 = 29;
const T_EX_PROJ: u32 = 30;
const T_EX_AUTO: u32 = 31;
/// A list of the `arity` terms before it (a predicate's argument list).
const T_LIST: u32 = 32;
const T_COROUTINE: u32 = 33;
const T_PAT: u32 = 34;
const T_RECORD: u32 = 35;
const T_UNION: u32 = 36;

// Predicate tags.
const P_TRAIT: u32 = 64;
const P_PROJ: u32 = 65;
const P_NORM: u32 = 66;
const P_TOUT: u32 = 68;
const P_ROUT: u32 = 69;
const P_WF: u32 = 70;
const P_CE: u32 = 71;
const P_SUB: u32 = 72;
const P_COERCE: u32 = 73;
const P_DYNC: u32 = 74;
const P_HOST: u32 = 75;
const P_CAHT: u32 = 76;
const P_EQ: u32 = 77;
/// `Pred::ConstHolds`: laid out as `P_TRAIT` (the list, the def, a flag word of 0).
const P_HOLDS: u32 = 78;
/// `Pred::ForFields`: container term, `args_after_self` list, header, then the trait def.
const P_FOR_FIELDS: u32 = 79;

fn header(tag: u32, arity: u32) -> u32 {
    tag | (arity << 8)
}

fn push_def(out: &mut Vec<u32, Alloc<'_>>, d: Def) {
    let k = d.0;
    out.push(k as u32);
    out.push((k >> 32) as u32);
    out.push((k >> 64) as u32);
    out.push((k >> 96) as u32);
}

fn read_def(w: &[u32], at: usize) -> Def {
    Def(u128::from(w[at]) | (u128::from(w[at + 1]) << 32) | (u128::from(w[at + 2]) << 64) | (u128::from(w[at + 3]) << 96))
}

fn var_kind_word(k: VarKind) -> u32 {
    match k {
        VarKind::Ty => 0,
        VarKind::Int => 1,
        VarKind::Float => 2,
        VarKind::Region => 3,
        VarKind::Const => 4,
    }
}

fn word_var_kind(w: u32) -> VarKind {
    match w {
        1 => VarKind::Int,
        2 => VarKind::Float,
        3 => VarKind::Region,
        4 => VarKind::Const,
        _ => VarKind::Ty,
    }
}

const INTS: [IntTy; 6] = [IntTy::I8, IntTy::I16, IntTy::I32, IntTy::I64, IntTy::I128, IntTy::Isize];
const UINTS: [UintTy; 6] = [UintTy::U8, UintTy::U16, UintTy::U32, UintTy::U64, UintTy::U128, UintTy::Usize];
const FLOATS: [FloatTy; 4] = [FloatTy::F16, FloatTy::F32, FloatTy::F64, FloatTy::F128];
const ALIASES: [AliasKind; 5] = [AliasKind::Projection, AliasKind::Inherent, AliasKind::Opaque, AliasKind::Free, AliasKind::Const];

fn mutbl_word(m: Mutbl) -> u32 {
    match m {
        Mutbl::Not => 0,
        Mutbl::Mut => 1,
    }
}

fn word_mutbl(w: u32) -> Mutbl {
    if w == 0 { Mutbl::Not } else { Mutbl::Mut }
}

/// The canonicalizer's state for one form: the variables met so far, in order.
pub struct Canon<'a> {
    /// (arena variable index, canonical index), in first-occurrence order.
    vars: Vec<(u32, u32), Alloc<'a>>,
    /// The variables' terms in the arena, by canonical index: the form's original values.
    pub orig: Vec<Ty, Alloc<'a>>,
    work: Vec<(Ty, bool), Alloc<'a>>,
}

impl<'a> Canon<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Canon<'a> {
        Canon { vars: Vec::new_in(alloc), orig: Vec::new_in(alloc), work: Vec::new_in(alloc) }
    }

    pub fn clear(&mut self) {
        self.vars.clear();
        self.orig.clear();
        self.work.clear();
    }

    fn var(&mut self, v: Var, t: Ty) -> u32 {
        for &(i, c) in self.vars.iter() {
            if i == v.index {
                return c;
            }
        }
        let c = self.orig.len() as u32;
        self.vars.push((v.index, c));
        self.orig.push(t);
        c
    }

    /// Writes `root` (resolved) to `out`.
    pub fn term<T: Tys>(&mut self, tys: &T, out: &mut Vec<u32, Alloc<'_>>, root: Ty) -> Result<(), SolveError> {
        let base = self.work.len();
        self.work.push((root, false));
        while self.work.len() > base {
            let Some((t, after)) = self.work.pop() else { break };
            if out.len() > MAX_WORDS {
                self.work.truncate(base);
                return Err(SolveError::Limit(Limit::CanonicalSize));
            }
            if after {
                emit(tys, out, tys.kind(t));
                continue;
            }
            let t = tys.shallow_resolve(t);
            let k = tys.kind(t);
            match k {
                TyKind::Infer(v) => match v.kind {
                    VarKind::Region => out.push(header(T_RE_ERASED, 0)),
                    _ => {
                        let c = self.var(v, t);
                        out.push(header(T_CANON, 0));
                        out.push(c);
                        out.push(var_kind_word(v.kind));
                    }
                },
                TyKind::ReParam(_) | TyKind::ReErased => out.push(header(T_RE_ERASED, 0)),
                _ => {
                    let c = crate::term::children(k);
                    self.work.push((t, true));
                    // Children are written in order: push them reversed.
                    match c {
                        crate::term::Children::None => {}
                        crate::term::Children::One(a) => self.work.push((a, false)),
                        crate::term::Children::Two(a, b) => {
                            self.work.push((b, false));
                            self.work.push((a, false));
                        }
                        crate::term::Children::List(l) => {
                            for i in (0..l.len).rev() {
                                self.work.push((tys.list_get(l, i), false));
                            }
                        }
                        crate::term::Children::ListThen(l, x) => {
                            self.work.push((x, false));
                            for i in (0..l.len).rev() {
                                self.work.push((tys.list_get(l, i), false));
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Writes the list `l` as its terms then a `T_LIST` header.
    pub fn list<T: Tys>(&mut self, tys: &T, out: &mut Vec<u32, Alloc<'_>>, l: List) -> Result<(), SolveError> {
        for i in 0..l.len {
            self.term(tys, out, tys.list_get(l, i))?;
        }
        out.push(header(T_LIST, l.len));
        Ok(())
    }

    /// Writes the predicate `p`.
    pub fn pred<T: Tys>(&mut self, tys: &T, out: &mut Vec<u32, Alloc<'_>>, p: Pred) -> Result<(), SolveError> {
        let two = |c: &mut Canon<'a>, out: &mut Vec<u32, Alloc<'_>>, a: Ty, b: Ty, tag: u32| -> Result<(), SolveError> {
            c.term(tys, out, a)?;
            c.term(tys, out, b)?;
            out.push(header(tag, 2));
            Ok(())
        };
        match p {
            Pred::Trait { trait_, args, polarity } => {
                self.list(tys, out, args)?;
                out.push(header(P_TRAIT, 1));
                push_def(out, trait_);
                out.push(match polarity {
                    Polarity::Positive => 0,
                    Polarity::Negative => 1,
                });
            }
            Pred::HostEffect { trait_, args, constness } => {
                self.list(tys, out, args)?;
                out.push(header(P_HOST, 1));
                push_def(out, trait_);
                out.push(match constness {
                    Constness::Const => 0,
                    Constness::Maybe => 1,
                });
            }
            Pred::ConstHolds { cond, args } => {
                self.list(tys, out, args)?;
                out.push(header(P_HOLDS, 1));
                push_def(out, cond);
                out.push(0);
            }
            Pred::ForFields { container, trait_, args_after_self } => {
                self.term(tys, out, container)?;
                self.list(tys, out, args_after_self)?;
                out.push(header(P_FOR_FIELDS, 2));
                push_def(out, trait_);
            }
            Pred::Projection { alias, term } => two(self, out, alias, term, P_PROJ)?,
            Pred::NormalizesTo { alias, term } => two(self, out, alias, term, P_NORM)?,
            Pred::TypeOutlives { ty, region } => two(self, out, ty, region, P_TOUT)?,
            Pred::RegionOutlives { a, b } => two(self, out, a, b, P_ROUT)?,
            Pred::Subtype { a, b } => two(self, out, a, b, P_SUB)?,
            Pred::Coerce { a, b } => two(self, out, a, b, P_COERCE)?,
            Pred::ConstArgHasType { ct, ty } => two(self, out, ct, ty, P_CAHT)?,
            Pred::Eq { a, b } => two(self, out, a, b, P_EQ)?,
            Pred::WellFormed(t) => {
                self.term(tys, out, t)?;
                out.push(header(P_WF, 1));
            }
            Pred::ConstEvaluatable(t) => {
                self.term(tys, out, t)?;
                out.push(header(P_CE, 1));
            }
            Pred::DynCompatible(d) => {
                out.push(header(P_DYNC, 0));
                push_def(out, d);
            }
        }
        if out.len() > MAX_WORDS {
            return Err(SolveError::Limit(Limit::CanonicalSize));
        }
        Ok(())
    }
}

/// Writes one level `k` (its children already written).
fn emit<T: Tys>(tys: &T, out: &mut Vec<u32, Alloc<'_>>, k: TyKind) {
    let _ = tys;
    match k {
        TyKind::Bool => out.push(header(T_BOOL, 0)),
        TyKind::Char => out.push(header(T_CHAR, 0)),
        TyKind::Int(i) => {
            out.push(header(T_INT, 0));
            out.push(i as u32);
        }
        TyKind::Uint(u) => {
            out.push(header(T_UINT, 0));
            out.push(u as u32);
        }
        TyKind::Float(f) => {
            out.push(header(T_FLOAT, 0));
            out.push(f as u32);
        }
        TyKind::Str => out.push(header(T_STR, 0)),
        TyKind::Never => out.push(header(T_NEVER, 0)),
        TyKind::Tuple(l) => out.push(header(T_TUPLE, l.len)),
        TyKind::Record(s, l) => {
            out.push(header(T_RECORD, l.len));
            let [b0, b1, b2, b3, b4, b5, b6, b7] = s.to_le_bytes();
            out.push(u32::from_le_bytes([b0, b1, b2, b3]));
            out.push(u32::from_le_bytes([b4, b5, b6, b7]));
        }
        TyKind::Union(l) => out.push(header(T_UNION, l.len)),
        TyKind::Ref(_, _, m) => {
            out.push(header(T_REF, 2));
            out.push(mutbl_word(m));
        }
        TyKind::RawPtr(_, m) => {
            out.push(header(T_PTR, 1));
            out.push(mutbl_word(m));
        }
        TyKind::Array(..) => out.push(header(T_ARRAY, 2)),
        TyKind::Slice(_) => out.push(header(T_SLICE, 1)),
        // Children as `children` gives them: the ranges' bounds, then the base.
        TyKind::Pat(_, l) => out.push(header(T_PAT, l.len + 1)),
        TyKind::Adt(d, l) => {
            out.push(header(T_ADT, l.len));
            push_def(out, d);
        }
        TyKind::Foreign(d) => {
            out.push(header(T_FOREIGN, 0));
            push_def(out, d);
        }
        TyKind::FnDef(d, l) => {
            out.push(header(T_FNDEF, l.len));
            push_def(out, d);
        }
        TyKind::FnPtr(l, h) => {
            out.push(header(T_FNPTR, l.len));
            out.push(u32::from(h.bits()));
        }
        TyKind::Closure(d, l) => {
            out.push(header(T_CLOSURE, l.len));
            push_def(out, d);
        }
        TyKind::Coroutine(d, l) => {
            out.push(header(T_COROUTINE, l.len));
            push_def(out, d);
        }
        TyKind::Dynamic(l, _) => out.push(header(T_DYN, l.len + 1)),
        TyKind::Alias(r, a, d, l) => {
            out.push(header(T_ALIAS, l.len));
            out.push(match r {
                Rigid::No => 0,
                Rigid::Yes => 1,
            } | ((a as u32) << 1));
            push_def(out, d);
        }
        TyKind::Param(i) => {
            out.push(header(T_PARAM, 0));
            out.push(i);
        }
        TyKind::Placeholder(u, i) => {
            out.push(header(T_PLACEHOLDER, 0));
            out.push(u);
            out.push(i);
        }
        TyKind::Bound(d, i) => {
            out.push(header(T_BOUND, 0));
            out.push(d);
            out.push(i);
        }
        TyKind::Canonical(i) => {
            // A canonical term in the arena (the solver never makes one; kept for totality).
            out.push(header(T_CANON, 0));
            out.push(i);
            out.push(0);
        }
        TyKind::Infer(_) => out.push(header(T_ERROR, 0)),
        TyKind::Error => out.push(header(T_ERROR, 0)),
        TyKind::ReStatic => out.push(header(T_RE_STATIC, 0)),
        TyKind::ReParam(_) | TyKind::ReErased => out.push(header(T_RE_ERASED, 0)),
        TyKind::ConstValue(t, v) => {
            out.push(header(T_CONST, 0));
            out.push(t.code());
            if let ValTy::Enum(d) = t {
                push_def(out, d);
            }
            for i in 0..4 {
                out.push((v >> (32 * i)) as u32);
            }
        }
        TyKind::ConstParam(i) => {
            out.push(header(T_CONST_PARAM, 0));
            out.push(i);
        }
        TyKind::ExTrait(d, l) => {
            out.push(header(T_EX_TRAIT, l.len));
            push_def(out, d);
        }
        TyKind::ExProj(d, l, _) => {
            out.push(header(T_EX_PROJ, l.len + 1));
            push_def(out, d);
        }
        TyKind::ExAuto(d) => {
            out.push(header(T_EX_AUTO, 0));
            push_def(out, d);
        }
    }
}

fn mode_word(m: TypingMode) -> u32 {
    match m {
        TypingMode::Analysis => 0,
        TypingMode::Coherence => 1,
        TypingMode::PostAnalysis => 2,
    }
}

/// Writes a goal's canonical input: `[mode, env, bound]` then the predicate.
pub fn goal_input<T: Tys>(
    tys: &T,
    c: &mut Canon<'_>,
    out: &mut Vec<u32, Alloc<'_>>,
    mode: TypingMode,
    goal: Goal,
) -> Result<(), SolveError> {
    c.clear();
    out.clear();
    out.push(mode_word(mode));
    out.push(goal.env.0);
    out.push(goal.bound);
    c.pred(tys, out, goal.pred)
}

/// The decoder's stacks.
pub struct Decode<'a> {
    tys: Vec<Ty, Alloc<'a>>,
    lists: Vec<List, Alloc<'a>>,
    kids: Vec<Ty, Alloc<'a>>,
    /// The arena term each canonical variable stands for (`Ty::NONE`: not yet made).
    pub slots: Vec<Ty, Alloc<'a>>,
}

impl<'a> Decode<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Decode<'a> {
        Decode { tys: Vec::new_in(alloc), lists: Vec::new_in(alloc), kids: Vec::new_in(alloc), slots: Vec::new_in(alloc) }
    }

    fn slot<T: Tys>(&mut self, tys: &mut T, i: u32, kind: VarKind) -> Result<Ty, SolveError> {
        // Variables are numbered by first occurrence, and a decoder kept across forms (one
        // certificate's rows, blaze-cert) numbers them across all of them: an index past
        // `MAX_VARS` is malformed input (a fetched certificate's words), refused before the
        // slot table grows to it.
        if i > MAX_VARS {
            return Err(SolveError::Malformed(Malformed::Canonical { at: i }));
        }
        while self.slots.len() <= i as usize {
            self.slots.push(Ty::NONE);
        }
        let s = self.slots[i as usize];
        if s != Ty::NONE {
            return Ok(s);
        }
        let v = tys.fresh(kind)?;
        self.slots[i as usize] = v;
        Ok(v)
    }

    fn pop_kids(&mut self, n: u32, at: usize) -> Result<(), SolveError> {
        if self.tys.len() < n as usize {
            return Err(SolveError::Malformed(Malformed::Canonical { at: at as u32 }));
        }
        let start = self.tys.len() - n as usize;
        self.kids.clear();
        self.kids.extend_from_slice(&self.tys[start..]);
        self.tys.truncate(start);
        Ok(())
    }

    /// Decodes `w` (terms, lists and at most one predicate, post-order) into the arena;
    /// returns the predicate if one was written last. Terms decoded are left on `self.tys`.
    pub fn run<T: Tys>(&mut self, tys: &mut T, w: &[u32]) -> Result<Option<Pred>, SolveError> {
        let bad = |at: usize| SolveError::Malformed(Malformed::Canonical { at: at as u32 });
        let mut i = 0usize;
        let mut pred = None;
        while i < w.len() {
            let h = w[i];
            let tag = h & 0xff;
            let n = h >> 8;
            let at = i;
            i += 1;
            let need = |len: usize| if at + 1 + len <= w.len() { Ok(()) } else { Err(bad(at)) };
            let k = match tag {
                T_BOOL => TyKind::Bool,
                T_CHAR => TyKind::Char,
                T_INT => {
                    need(1)?;
                    i += 1;
                    TyKind::Int(*INTS.get(w[at + 1] as usize).ok_or(bad(at))?)
                }
                T_UINT => {
                    need(1)?;
                    i += 1;
                    TyKind::Uint(*UINTS.get(w[at + 1] as usize).ok_or(bad(at))?)
                }
                T_FLOAT => {
                    need(1)?;
                    i += 1;
                    TyKind::Float(*FLOATS.get(w[at + 1] as usize).ok_or(bad(at))?)
                }
                T_STR => TyKind::Str,
                T_NEVER => TyKind::Never,
                T_TUPLE => {
                    self.pop_kids(n, at)?;
                    TyKind::Tuple(tys.mk_list(&self.kids)?)
                }
                T_RECORD => {
                    need(2)?;
                    i += 2;
                    let lo = *w.get(at + 1).ok_or(bad(at))?;
                    let hi = *w.get(at + 2).ok_or(bad(at))?;
                    let s = u64::from(lo) | (u64::from(hi) << 32);
                    self.pop_kids(n, at)?;
                    TyKind::Record(s, tys.mk_list(&self.kids)?)
                }
                T_UNION => {
                    self.pop_kids(n, at)?;
                    TyKind::Union(tys.mk_list(&self.kids)?)
                }
                T_REF => {
                    need(1)?;
                    i += 1;
                    self.pop_kids(2, at)?;
                    TyKind::Ref(self.kids[0], self.kids[1], word_mutbl(w[at + 1]))
                }
                T_PTR => {
                    need(1)?;
                    i += 1;
                    self.pop_kids(1, at)?;
                    TyKind::RawPtr(self.kids[0], word_mutbl(w[at + 1]))
                }
                T_ARRAY => {
                    self.pop_kids(2, at)?;
                    TyKind::Array(self.kids[0], self.kids[1])
                }
                T_SLICE => {
                    self.pop_kids(1, at)?;
                    TyKind::Slice(self.kids[0])
                }
                T_ADT | T_FNDEF | T_CLOSURE | T_COROUTINE | T_EX_TRAIT => {
                    need(4)?;
                    i += 4;
                    let d = read_def(w, at + 1);
                    self.pop_kids(n, at)?;
                    let l = tys.mk_list(&self.kids)?;
                    match tag {
                        T_ADT => TyKind::Adt(d, l),
                        T_FNDEF => TyKind::FnDef(d, l),
                        T_CLOSURE => TyKind::Closure(d, l),
                        T_COROUTINE => TyKind::Coroutine(d, l),
                        _ => TyKind::ExTrait(d, l),
                    }
                }
                T_FOREIGN | T_EX_AUTO => {
                    need(4)?;
                    i += 4;
                    let d = read_def(w, at + 1);
                    if tag == T_FOREIGN { TyKind::Foreign(d) } else { TyKind::ExAuto(d) }
                }
                T_FNPTR => {
                    need(1)?;
                    i += 1;
                    let h = u8::try_from(w[at + 1]).ok().and_then(FnPtrHeader::from_bits).ok_or(bad(at))?;
                    self.pop_kids(n, at)?;
                    TyKind::FnPtr(tys.mk_list(&self.kids)?, h)
                }
                T_PAT => {
                    if n == 0 {
                        return Err(bad(at));
                    }
                    self.pop_kids(n, at)?;
                    let base = self.kids[n as usize - 1];
                    TyKind::Pat(base, tys.mk_list(&self.kids[..n as usize - 1])?)
                }
                T_DYN | T_EX_PROJ => {
                    if n == 0 {
                        return Err(bad(at));
                    }
                    let d = if tag == T_EX_PROJ {
                        need(4)?;
                        i += 4;
                        read_def(w, at + 1)
                    } else {
                        Def(0)
                    };
                    self.pop_kids(n, at)?;
                    let last = self.kids[n as usize - 1];
                    let l = tys.mk_list(&self.kids[..n as usize - 1])?;
                    if tag == T_DYN { TyKind::Dynamic(l, last) } else { TyKind::ExProj(d, l, last) }
                }
                T_ALIAS => {
                    need(5)?;
                    i += 5;
                    let rk = w[at + 1];
                    let r = if rk & 1 == 1 { Rigid::Yes } else { Rigid::No };
                    let a = *ALIASES.get((rk >> 1) as usize).ok_or(bad(at))?;
                    let d = read_def(w, at + 2);
                    self.pop_kids(n, at)?;
                    TyKind::Alias(r, a, d, tys.mk_list(&self.kids)?)
                }
                T_PARAM => {
                    need(1)?;
                    i += 1;
                    TyKind::Param(w[at + 1])
                }
                T_PLACEHOLDER => {
                    need(2)?;
                    i += 2;
                    TyKind::Placeholder(w[at + 1], w[at + 2])
                }
                T_BOUND => {
                    need(2)?;
                    i += 2;
                    TyKind::Bound(w[at + 1], w[at + 2])
                }
                T_CANON => {
                    need(2)?;
                    i += 2;
                    let v = self.slot(tys, w[at + 1], word_var_kind(w[at + 2]))?;
                    self.tys.push(v);
                    continue;
                }
                T_ERROR => TyKind::Error,
                T_RE_STATIC => TyKind::ReStatic,
                T_RE_ERASED => TyKind::ReErased,
                T_CONST => {
                    need(1)?;
                    // An enum's definition: four words between the code and the bits.
                    let (t, bits_at) = if w[at + 1] == ValTy::ENUM_CODE {
                        need(9)?;
                        (ValTy::Enum(read_def(w, at + 2)), at + 6)
                    } else {
                        need(5)?;
                        let Some(t) = ValTy::from_code(w[at + 1]) else { return Err(bad(at)) };
                        (t, at + 2)
                    };
                    i += bits_at + 4 - (at + 1);
                    let mut v = 0u128;
                    for k in 0..4 {
                        v |= u128::from(w[bits_at + k]) << (32 * k);
                    }
                    TyKind::ConstValue(t, v)
                }
                T_CONST_PARAM => {
                    need(1)?;
                    i += 1;
                    TyKind::ConstParam(w[at + 1])
                }
                T_LIST => {
                    self.pop_kids(n, at)?;
                    let l = tys.mk_list(&self.kids)?;
                    self.lists.push(l);
                    continue;
                }
                _ => {
                    pred = Some(self.pred(tys, w, at, tag, &mut i)?);
                    continue;
                }
            };
            let t = tys.mk(k)?;
            self.tys.push(t);
        }
        Ok(pred)
    }

    fn pred<T: Tys>(&mut self, tys: &mut T, w: &[u32], at: usize, tag: u32, i: &mut usize) -> Result<Pred, SolveError> {
        let _ = tys;
        let bad = SolveError::Malformed(Malformed::Canonical { at: at as u32 });
        let two = |s: &mut Self| -> Result<(Ty, Ty), SolveError> {
            s.pop_kids(2, at)?;
            Ok((s.kids[0], s.kids[1]))
        };
        Ok(match tag {
            P_TRAIT | P_HOST | P_HOLDS => {
                if at + 5 >= w.len() {
                    return Err(bad);
                }
                let d = read_def(w, at + 1);
                let flag = w[at + 5];
                *i += 5;
                let args = self.lists.pop().ok_or(bad)?;
                if tag == P_TRAIT {
                    Pred::Trait {
                        trait_: d,
                        args,
                        polarity: if flag == 0 { Polarity::Positive } else { Polarity::Negative },
                    }
                } else if tag == P_HOLDS {
                    Pred::ConstHolds { cond: d, args }
                } else {
                    Pred::HostEffect {
                        trait_: d,
                        args,
                        constness: if flag == 0 { Constness::Const } else { Constness::Maybe },
                    }
                }
            }
            P_PROJ => {
                let (alias, term) = two(self)?;
                Pred::Projection { alias, term }
            }
            P_NORM => {
                let (alias, term) = two(self)?;
                Pred::NormalizesTo { alias, term }
            }
            P_TOUT => {
                let (ty, region) = two(self)?;
                Pred::TypeOutlives { ty, region }
            }
            P_ROUT => {
                let (a, b) = two(self)?;
                Pred::RegionOutlives { a, b }
            }
            P_SUB => {
                let (a, b) = two(self)?;
                Pred::Subtype { a, b }
            }
            P_COERCE => {
                let (a, b) = two(self)?;
                Pred::Coerce { a, b }
            }
            P_CAHT => {
                let (ct, ty) = two(self)?;
                Pred::ConstArgHasType { ct, ty }
            }
            P_EQ => {
                let (a, b) = two(self)?;
                Pred::Eq { a, b }
            }
            P_WF => {
                self.pop_kids(1, at)?;
                Pred::WellFormed(self.kids[0])
            }
            P_CE => {
                self.pop_kids(1, at)?;
                Pred::ConstEvaluatable(self.kids[0])
            }
            P_DYNC => {
                if at + 5 > w.len() {
                    return Err(bad);
                }
                *i += 4;
                Pred::DynCompatible(read_def(w, at + 1))
            }
            P_FOR_FIELDS => {
                if at + 5 > w.len() {
                    return Err(bad);
                }
                let trait_ = read_def(w, at + 1);
                *i += 4;
                self.pop_kids(1, at)?;
                let container = self.kids[0];
                let args_after_self = self.lists.pop().ok_or(bad)?;
                Pred::ForFields { container, trait_, args_after_self }
            }
            _ => return Err(bad),
        })
    }

    pub fn reset(&mut self) {
        self.tys.clear();
        self.lists.clear();
        self.slots.clear();
    }

    pub fn take_ty(&mut self) -> Option<Ty> {
        self.tys.pop()
    }
}

/// A goal decoded from its canonical input with fresh variables: what a frame proves.
pub fn instantiate_input<T: Tys>(tys: &mut T, d: &mut Decode<'_>, w: &[u32]) -> Result<(TypingMode, Goal), SolveError> {
    if w.len() < 3 {
        return Err(SolveError::Malformed(Malformed::Canonical { at: 0 }));
    }
    d.reset();
    // Exactly the words `mode_word` writes; any other is malformed, never a mode by default.
    let mode = match w[0] {
        0 => TypingMode::Analysis,
        1 => TypingMode::Coherence,
        2 => TypingMode::PostAnalysis,
        _ => return Err(SolveError::Malformed(Malformed::Canonical { at: 0 })),
    };
    let env = EnvId(w[1]);
    let bound = w[2];
    let pred = d.run(tys, &w[3..])?.ok_or(SolveError::Malformed(Malformed::Canonical { at: 3 }))?;
    Ok((mode, Goal { env, pred, bound }))
}

/// A response's id: an index into a [`Responses`] pool; the top bit says which pool (the
/// frames' scratch, or the cache's).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RespId(pub u32);

impl RespId {
    pub const CACHED: u32 = 1 << 31;

    pub fn is_cached(self) -> bool {
        self.0 & RespId::CACHED != 0
    }
}

/// A response's row.
#[derive(Clone, Copy, Debug)]
struct Row {
    start: u32,
    len: u32,
    certainty: Certainty,
    via: Via,
    identity: bool,
}

/// One pool of responses' words.
pub struct Pool<'a> {
    words: Vec<u32, Alloc<'a>>,
    rows: Vec<Row, Alloc<'a>>,
}

impl<'a> Pool<'a> {
    fn new_in(alloc: Alloc<'a>) -> Pool<'a> {
        Pool { words: Vec::new_in(alloc), rows: Vec::new_in(alloc) }
    }
}

/// Responses, in two pools: `scratch`, a stack of candidates' responses the frames truncate as
/// they pop, and `cached`, the append-only answers of the cache.
///
/// A response's words: `[nvals]` (`u32::MAX`: the identity), then per value `[len, words..]`,
/// then `[ngoals]` and per goal `[source, bound, len, words..]`. Certainty and `via` are on
/// the row.
pub struct Responses<'a> {
    pub(crate) scratch: Pool<'a>,
    pub(crate) cached: Pool<'a>,
}

const IDENTITY: u32 = u32::MAX;

fn source_word(s: GoalSource) -> u32 {
    s as u32
}

fn word_source(w: u32) -> GoalSource {
    match w {
        1 => GoalSource::ImplWhereBound,
        2 => GoalSource::AliasBoundConstCondition,
        3 => GoalSource::InstantiateHigherRanked,
        4 => GoalSource::AliasWellFormed,
        5 => GoalSource::NormalizeGoal,
        6 => GoalSource::TypeRelating,
        _ => GoalSource::Misc,
    }
}

/// A nested goal handed back in a response.
#[derive(Clone, Copy, Debug)]
pub struct RespGoal {
    pub source: GoalSource,
    pub bound: u32,
    pub pred: Pred,
}

impl<'a> Responses<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Responses<'a> {
        Responses { scratch: Pool::new_in(alloc), cached: Pool::new_in(alloc) }
    }

    fn row(&self, r: RespId) -> Row {
        if r.is_cached() { self.cached.rows[(r.0 & !RespId::CACHED) as usize] } else { self.scratch.rows[r.0 as usize] }
    }

    /// A shared goal's answer taken into the cached pool (share.rs: a hit's response, its
    /// words as the table holds them).
    pub(crate) fn push_cached(&mut self, words: &[u32], certainty: Certainty, via: Via) -> Result<RespId, SolveError> {
        self.push(true, words, certainty, via)
    }

    /// A response's words (what a shared goal's answer holds: share.rs).
    pub(crate) fn words_of(&self, r: RespId) -> &[u32] {
        let row = self.row(r);
        let pool = if r.is_cached() { &self.cached } else { &self.scratch };
        &pool.words[row.start as usize..(row.start + row.len) as usize]
    }

    pub fn certainty(&self, r: RespId) -> Certainty {
        self.row(r).certainty
    }

    pub fn via(&self, r: RespId) -> Via {
        self.row(r).via
    }

    /// No inference constraints and no goals.
    pub fn is_identity(&self, r: RespId) -> bool {
        self.row(r).identity
    }

    /// No inference constraints, nested goals aside (`has_only_region_constraints`: regions
    /// are erased here): value `i` is canonical variable `i`, for every `i`.
    pub fn vars_identity(&self, r: RespId) -> bool {
        if self.is_identity(r) {
            return true;
        }
        let w = self.words_of(r);
        let Some(&n) = w.first() else { return false };
        let mut at = 1usize;
        for i in 0..n {
            let Some(&len) = w.get(at) else { return false };
            let len = len as usize;
            let Some(v) = w.get(at + 1..at + 1 + len) else { return false };
            if !(len == 3 && v[0] == header(T_CANON, 0) && v[1] == i) {
                return false;
            }
            at += 1 + len;
        }
        true
    }

    /// Equal certainty and words (`via` aside).
    pub fn equal(&self, a: RespId, b: RespId) -> bool {
        self.certainty(a) == self.certainty(b) && self.words_of(a) == self.words_of(b)
    }

    /// The scratch pool's marks, to truncate back to.
    pub fn scratch_mark(&self) -> (u32, u32) {
        (self.scratch.rows.len() as u32, self.scratch.words.len() as u32)
    }

    pub fn truncate_scratch(&mut self, mark: (u32, u32)) {
        self.scratch.rows.truncate(mark.0 as usize);
        self.scratch.words.truncate(mark.1 as usize);
    }

    fn push(&mut self, cached: bool, words: &[u32], certainty: Certainty, via: Via) -> Result<RespId, SolveError> {
        let identity = words.len() == 2 && words[0] == IDENTITY && words[1] == 0;
        let pool = if cached { &mut self.cached } else { &mut self.scratch };
        let id = crate::error::row(pool.rows.len())?;
        let start = crate::error::row(pool.words.len())?;
        pool.words.extend_from_slice(words);
        pool.rows.push(Row { start, len: words.len() as u32, certainty, via, identity });
        Ok(RespId(if cached { id | RespId::CACHED } else { id }))
    }

    /// The identity response with `certainty`.
    pub fn no_constraints(&mut self, _vars: u32, certainty: Certainty, via: Via) -> Result<RespId, SolveError> {
        self.push(false, &[IDENTITY, 0], certainty, via)
    }

    /// `r` with `via` set (a copy in scratch).
    pub fn with_via(&mut self, r: RespId, via: Via) -> Result<RespId, SolveError> {
        if self.via(r) == via {
            return Ok(r);
        }
        let row = self.row(r);
        let id = crate::error::row(self.scratch.rows.len())?;
        let start = crate::error::row(self.scratch.words.len())?;
        let (s, e) = (row.start as usize, (row.start + row.len) as usize);
        if r.is_cached() {
            self.scratch.words.extend_from_slice(&self.cached.words[s..e]);
        } else {
            self.scratch.words.reserve(row.len as usize);
            for k in s..e {
                let w = self.scratch.words[k];
                self.scratch.words.push(w);
            }
        }
        self.scratch.rows.push(Row { start, len: row.len, certainty: row.certainty, via, identity: row.identity });
        Ok(RespId(id))
    }

    /// Copies `r` into the cached pool (a frame's final answer).
    pub fn keep(&mut self, r: RespId) -> Result<RespId, SolveError> {
        if r.is_cached() {
            return Ok(r);
        }
        let row = self.row(r);
        let start = crate::error::row(self.cached.words.len())?;
        let id = crate::error::row(self.cached.rows.len())?;
        let (s, e) = (row.start as usize, (row.start + row.len) as usize);
        self.cached.words.extend_from_slice(&self.scratch.words[s..e]);
        self.cached.rows.push(Row { start, ..row });
        Ok(RespId(id | RespId::CACHED))
    }

    /// Makes a response: `vals` are the frame's input variables (resolved by the canonicalizer),
    /// `goals` the goals handed back (for `NormalizesTo`).
    pub fn make<T: Tys>(
        &mut self,
        tys: &T,
        c: &mut Canon<'_>,
        tmp: &mut Vec<u32, Alloc<'_>>,
        vals: &[Ty],
        goals: &[RespGoal],
        certainty: Certainty,
        via: Via,
    ) -> Result<RespId, SolveError> {
        c.clear();
        tmp.clear();
        tmp.push(vals.len() as u32);
        for &v in vals {
            let at = tmp.len();
            tmp.push(0);
            c.term(tys, tmp, v)?;
            tmp[at] = (tmp.len() - at - 1) as u32;
        }
        tmp.push(goals.len() as u32);
        for g in goals {
            tmp.push(source_word(g.source));
            tmp.push(g.bound);
            let at = tmp.len();
            tmp.push(0);
            c.pred(tys, tmp, g.pred)?;
            tmp[at] = (tmp.len() - at - 1) as u32;
        }
        // The identity: no goals, and value i is canonical variable i, for every i.
        let mut identity = goals.is_empty();
        if identity {
            let mut at = 1usize;
            for i in 0..vals.len() {
                let len = tmp[at] as usize;
                let w = &tmp[at + 1..at + 1 + len];
                if !(len == 3 && w[0] == header(T_CANON, 0) && w[1] == i as u32) {
                    identity = false;
                    break;
                }
                at += 1 + len;
            }
        }
        if identity {
            return self.push(false, &[IDENTITY, 0], certainty, via);
        }
        self.push(false, tmp, certainty, via)
    }

    /// Applies `r` in the caller: each original value `orig[i]` is related with the response's
    /// value `i` by structure (canonical/mod.rs `instantiate_and_apply_query_response`,
    /// `unify_query_var_values`), a value that is a bare canonical variable not met before
    /// taking the original term itself (`compute_query_response_instantiation_values`). The
    /// handed-back goals are pushed to `goals`. `Ok(false)`: a value does not relate, which a
    /// correct solver never answers.
    pub fn apply<T: Tys>(
        &self,
        tys: &mut T,
        d: &mut Decode<'_>,
        deferred: &mut Vec<(Ty, Ty), Alloc<'_>>,
        r: RespId,
        orig: &[Ty],
        goals: &mut Vec<RespGoal, Alloc<'_>>,
    ) -> Result<bool, SolveError> {
        if self.is_identity(r) {
            return Ok(true);
        }
        let w = self.words_of(r);
        let bad = |at: usize| SolveError::Malformed(Malformed::Canonical { at: at as u32 });
        d.reset();
        let n = *w.first().ok_or(bad(0))? as usize;
        if n != orig.len() {
            return Err(bad(0));
        }
        // First pass: bare canonical variables take the original value.
        let mut at = 1usize;
        for &o in orig {
            let len = *w.get(at).ok_or(bad(at))? as usize;
            if at + 1 + len > w.len() {
                return Err(bad(at));
            }
            let v = &w[at + 1..at + 1 + len];
            if len == 3 && v[0] == header(T_CANON, 0) {
                let j = v[1] as usize;
                while d.slots.len() <= j {
                    d.slots.push(Ty::NONE);
                }
                if d.slots[j] == Ty::NONE {
                    d.slots[j] = o;
                }
            }
            at += 1 + len;
        }
        // Second pass: relate the rest.
        let mut at = 1usize;
        for &o in orig {
            let len = w[at] as usize;
            let v = &w[at + 1..at + 1 + len];
            at += 1 + len;
            d.tys.clear();
            d.lists.clear();
            d.run(tys, v)?;
            let t = d.take_ty().ok_or(bad(at))?;
            if t == o {
                continue;
            }
            let base = deferred.len();
            let ok = tys.unify(o, t, Relate::Structural, deferred)?;
            deferred.truncate(base);
            if !ok {
                return Ok(false);
            }
        }
        let ng = *w.get(at).ok_or(bad(at))? as usize;
        at += 1;
        for _ in 0..ng {
            if at + 3 > w.len() {
                return Err(bad(at));
            }
            let source = word_source(w[at]);
            let bound = w[at + 1];
            let len = w[at + 2] as usize;
            at += 3;
            if at + len > w.len() {
                return Err(bad(at));
            }
            d.tys.clear();
            d.lists.clear();
            let pred = d.run(tys, &w[at..at + len])?.ok_or(bad(at))?;
            at += len;
            goals.push(RespGoal { source, bound, pred });
        }
        Ok(true)
    }
}
