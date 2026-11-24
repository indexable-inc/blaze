//! blaze-solve's `Tys` over blaze-ty's arena: the one term store of a body's check.
//!
//! Handles: a solver `Ty` is a blaze-ty node's bits (its class in the low two bits: type,
//! region, const), so `kind` reads the row its class names. A solver `List` is a span of
//! blaze-ty's list pool, `start` the pool position (`Tys::list_span`), so a list's tail (a
//! closure's or a `dyn` projection's) is a span too and is never interned to be named. A solver
//! `Def` is the item's 128-bit key; blaze-ty's item column holds it in bytes 0..16 of its
//! `ItemKey`, the rest zero ([`item_key`], [`def_of`]).
//!
//! Sorts: the solver's terms are one sort, blaze-ty's three. Regions are `ReStatic`,
//! `ReParam`, `ReErased` and region variables; consts are `ConstValue`, `ConstParam`, const
//! variables and `Alias(_, Const, ..)`. A placeholder, a bound variable and an error are made
//! as types: the solver binds and instantiates only type variables (regions are erased:
//! solve.md "Not ported"), so it never makes a region or const of those. A child of the wrong
//! sort (a type as a reference's region) is `Malformed::Sort`, never coerced (rule 3).
//!
//! Variables: the solver's variable index is unique across kinds: blaze-ty's index times 8
//! plus the kind's code ([`var_index`]), and the binding log is blaze-ty's, so wake lists see
//! every binding the arena makes, relating's included.

use blaze_own::{Alloc, Vec};
use blaze_solve::error::{Malformed, Met, SolveError};
use blaze_solve::term::{
    self as s, AliasKind as SAlias, Def as SDef, Flags as SFlags, List as SList, Markers, Rigid, Ty as STy,
    TyKind as K, Var as SVar, VarKind as SVarKind,
};
use blaze_solve::tys::Relate;
use blaze_ty::{
    AliasKind, AllInvariant, Arg, Const, ConstKind, FloatTy, FnHeader, IntTy, List, Mutability, Node, PredKind,
    Region, RegionKind, Ty, TyCx, TyKind, UintTy, Universe, VarId, VarKind,
};

/// The solver's view of one task's blaze-ty context.
pub struct Terms<'a> {
    pub cx: TyCx<'a>,
    /// Scratch: a list's elements while it is interned.
    args: Vec<Arg, Alloc<'a>>,
    /// Scratch: the goals relating emitted.
    goals: Vec<blaze_ty::Pred, Alloc<'a>>,
}

/// The arena key of a solver def.
pub fn item_key(d: u128) -> blaze_id::ItemKey {
    let mut b = [0u8; 32];
    b[..16].copy_from_slice(&d.to_le_bytes());
    blaze_id::ItemKey(blaze_ids::ContentId(b))
}

/// The solver def of an arena key (bytes 0..16).
pub fn def_of(k: blaze_id::ItemKey) -> u128 {
    let mut lo = [0u8; 16];
    lo.copy_from_slice(&k.0.0[..16]);
    u128::from_le_bytes(lo)
}

const fn kind_code(k: VarKind) -> u32 {
    match k {
        VarKind::Ty => 0,
        VarKind::Int => 1,
        VarKind::Float => 2,
        VarKind::Region => 3,
        VarKind::Const => 4,
    }
}

/// The solver's index of a blaze-ty variable: unique across kinds.
pub const fn var_index(v: VarId) -> u32 {
    v.index * 8 + kind_code(v.kind)
}

/// The blaze-ty variable of a solver index.
pub const fn var_of_index(i: u32) -> Option<VarId> {
    let kind = match i % 8 {
        0 => VarKind::Ty,
        1 => VarKind::Int,
        2 => VarKind::Float,
        3 => VarKind::Region,
        4 => VarKind::Const,
        _ => return None,
    };
    Some(VarId { kind, index: i / 8 })
}

fn svar(kind: VarKind, index: u32) -> K {
    let sk = match kind {
        VarKind::Ty => SVarKind::Ty,
        VarKind::Int => SVarKind::Int,
        VarKind::Float => SVarKind::Float,
        VarKind::Region => SVarKind::Region,
        VarKind::Const => SVarKind::Const,
    };
    K::Infer(SVar { kind: sk, index: var_index(VarId { kind, index }) })
}

pub(crate) fn s_int(i: IntTy) -> s::IntTy {
    match i {
        IntTy::Isize => s::IntTy::Isize,
        IntTy::I8 => s::IntTy::I8,
        IntTy::I16 => s::IntTy::I16,
        IntTy::I32 => s::IntTy::I32,
        IntTy::I64 => s::IntTy::I64,
        IntTy::I128 => s::IntTy::I128,
    }
}

pub(crate) fn b_int(i: s::IntTy) -> IntTy {
    match i {
        s::IntTy::Isize => IntTy::Isize,
        s::IntTy::I8 => IntTy::I8,
        s::IntTy::I16 => IntTy::I16,
        s::IntTy::I32 => IntTy::I32,
        s::IntTy::I64 => IntTy::I64,
        s::IntTy::I128 => IntTy::I128,
    }
}

pub(crate) fn s_uint(i: UintTy) -> s::UintTy {
    match i {
        UintTy::Usize => s::UintTy::Usize,
        UintTy::U8 => s::UintTy::U8,
        UintTy::U16 => s::UintTy::U16,
        UintTy::U32 => s::UintTy::U32,
        UintTy::U64 => s::UintTy::U64,
        UintTy::U128 => s::UintTy::U128,
    }
}

pub(crate) fn b_uint(i: s::UintTy) -> UintTy {
    match i {
        s::UintTy::Usize => UintTy::Usize,
        s::UintTy::U8 => UintTy::U8,
        s::UintTy::U16 => UintTy::U16,
        s::UintTy::U32 => UintTy::U32,
        s::UintTy::U64 => UintTy::U64,
        s::UintTy::U128 => UintTy::U128,
    }
}

pub(crate) fn s_float(f: FloatTy) -> s::FloatTy {
    match f {
        FloatTy::F16 => s::FloatTy::F16,
        FloatTy::F32 => s::FloatTy::F32,
        FloatTy::F64 => s::FloatTy::F64,
        FloatTy::F128 => s::FloatTy::F128,
    }
}

pub(crate) fn b_float(f: s::FloatTy) -> FloatTy {
    match f {
        s::FloatTy::F16 => FloatTy::F16,
        s::FloatTy::F32 => FloatTy::F32,
        s::FloatTy::F64 => FloatTy::F64,
        s::FloatTy::F128 => FloatTy::F128,
    }
}

/// The solver's type of a const value of type `t`; `None` when `t` is not one a const generic
/// may have (the Reference, "Const generics": integers, `char` and `bool`).
pub(crate) fn s_val_ty(t: TyKind) -> Option<s::ValTy> {
    match t {
        TyKind::Bool => Some(s::ValTy::Bool),
        TyKind::Char => Some(s::ValTy::Char),
        TyKind::Int(i) => Some(s::ValTy::Int(s_int(i))),
        TyKind::Uint(u) => Some(s::ValTy::Uint(s_uint(u))),
        _ => None,
    }
}

/// blaze-ty's type of a solver const value's type.
pub(crate) fn b_val_ty(t: s::ValTy) -> Ty {
    match t {
        s::ValTy::Bool => Ty::BOOL,
        s::ValTy::Char => Ty::CHAR,
        s::ValTy::Int(i) => Ty::int(b_int(i)),
        s::ValTy::Uint(u) => Ty::uint(b_uint(u)),
        // Interned with its definition by the one caller that sees one (`K::ConstValue`).
        s::ValTy::Enum(_) => Ty::ERROR,
    }
}

/// blaze-ty's fn pointer header from the solver's: the ABI's numbering is
/// `FnPtrHeader::bits`'s low six bits (blaze-ty: "the ABI's numbering is the front end's").
/// Lifetimes are erased here: no bound variables.
pub(crate) fn b_header(h: s::FnPtrHeader) -> FnHeader {
    FnHeader::new(h.bits() & 0x3f, h.safety == s::Safety::Unsafe, h.c_variadic, 0)
}

/// The inverse of [`b_header`]. A header this crate never made (an unknown ABI number) is a
/// broken invariant of the arena, not an input.
pub(crate) fn s_header(h: FnHeader) -> s::FnPtrHeader {
    let bits = h.abi() | if h.c_variadic() { 0x40 } else { 0 } | if h.is_unsafe() { 0x80 } else { 0 };
    match s::FnPtrHeader::from_bits(bits) {
        Some(x) if h.abi() < 0x40 => x,
        _ => panic!("blaze-typeck: a fn pointer header it never made: {h:?}"),
    }
}

fn s_mut(m: Mutability) -> s::Mutbl {
    match m {
        Mutability::Not => s::Mutbl::Not,
        Mutability::Mut => s::Mutbl::Mut,
    }
}

fn b_mut(m: s::Mutbl) -> Mutability {
    match m {
        s::Mutbl::Not => Mutability::Not,
        s::Mutbl::Mut => Mutability::Mut,
    }
}

fn s_alias(k: AliasKind) -> SAlias {
    match k {
        AliasKind::Projection => SAlias::Projection,
        AliasKind::Inherent => SAlias::Inherent,
        AliasKind::Opaque => SAlias::Opaque,
        AliasKind::Free => SAlias::Free,
    }
}

/// `term` refused where another sort was wanted: `met` is what it held, the caller's line says
/// which question refused it (the solver's driver adds the goal).
#[track_caller]
fn sort(term: u32, met: Met) -> SolveError {
    SolveError::Malformed(Malformed::Sort { term, met, at: core::panic::Location::caller(), goal: None })
}

/// What a node is, for `Malformed::Sort`.
fn met(n: Node) -> Met {
    if n.as_ty().is_some() {
        Met::Ty
    } else if n.as_region().is_some() {
        Met::Region
    } else if n.as_const().is_some() {
        Met::Const
    } else {
        Met::List
    }
}

impl<'a> Terms<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Terms<'a> {
        Terms { cx: TyCx::new_in(alloc), args: Vec::new_in(alloc), goals: Vec::new_in(alloc) }
    }

    #[track_caller]
    fn node(&self, t: STy) -> Result<Node, SolveError> {
        match self.cx.tys.node_at(t.0) {
            Some(n) => Ok(n),
            None => Err(sort(t.0, Met::Nothing)),
        }
    }

    /// The node of `t`, refused as `Sort` (naming what it held) unless `want` takes it.
    #[track_caller]
    fn node_as<X>(&self, t: STy, want: fn(Node) -> Option<X>) -> Result<X, SolveError> {
        let n = self.node(t)?;
        match want(n) {
            Some(x) => Ok(x),
            None => Err(sort(t.0, met(n))),
        }
    }

    #[track_caller]
    pub(crate) fn ty(&self, t: STy) -> Result<Ty, SolveError> {
        self.node_as(t, Node::as_ty)
    }

    #[track_caller]
    fn region(&self, t: STy) -> Result<Region, SolveError> {
        self.node_as(t, Node::as_region)
    }

    #[track_caller]
    fn konst(&self, t: STy) -> Result<Const, SolveError> {
        self.node_as(t, Node::as_const)
    }

    fn sdef(&self, d: blaze_ty::Def) -> SDef {
        SDef(def_of(self.cx.tys.item_key(d)))
    }

    pub(crate) fn bdef(&mut self, d: SDef) -> blaze_ty::Def {
        self.cx.tys.def(item_key(d.0))
    }

    pub(crate) fn slist(&self, l: List) -> SList {
        let (start, len) = self.cx.tys.list_span(l);
        if len == 0 { SList::EMPTY } else { SList { start, len } }
    }

    /// The arena list of a solver list (interned: equal elements give the arena's own list).
    pub(crate) fn blist(&mut self, l: SList) -> Result<List, SolveError> {
        self.args.clear();
        for i in 0..l.len {
            let a = self.cx.tys.pool_get(l.start + i).ok_or(sort(l.start + i, Met::Nothing))?;
            self.args.push(a);
        }
        Ok(self.cx.tys.mk_list(&self.args))
    }

    /// The list `l` without its last element, as a span.
    fn init(l: SList) -> SList {
        if l.len <= 1 { SList::EMPTY } else { SList { start: l.start, len: l.len - 1 } }
    }

    fn last(&self, l: SList) -> STy {
        match l.len.checked_sub(1).and_then(|i| self.cx.tys.pool_get(l.start + i)) {
            Some(a) => STy(a.bits()),
            None => STy::NONE,
        }
    }

    fn ty_kind(&self, t: Ty) -> K {
        match self.cx.tys.ty_kind(t) {
            TyKind::Bool => K::Bool,
            TyKind::Char => K::Char,
            TyKind::Int(i) => K::Int(s_int(i)),
            TyKind::Uint(u) => K::Uint(s_uint(u)),
            TyKind::Float(f) => K::Float(s_float(f)),
            TyKind::Str => K::Str,
            TyKind::Never => K::Never,
            TyKind::Adt(d, l) => K::Adt(self.sdef(d), self.slist(l)),
            TyKind::Foreign(d) => K::Foreign(self.sdef(d)),
            TyKind::Array(e, n) => K::Array(STy(e.node().bits()), STy(n.node().bits())),
            TyKind::Slice(e) => K::Slice(STy(e.node().bits())),
            TyKind::Pat(b, l) => K::Pat(STy(b.node().bits()), self.slist(l)),
            TyKind::RawPtr(p, m) => K::RawPtr(STy(p.node().bits()), s_mut(m)),
            TyKind::Ref(r, p, m) => K::Ref(STy(r.node().bits()), STy(p.node().bits()), s_mut(m)),
            TyKind::FnDef(d, l) => K::FnDef(self.sdef(d), self.slist(l)),
            TyKind::FnPtr(l, h) => K::FnPtr(self.slist(l), s_header(h)),
            TyKind::Dynamic(l, r) => K::Dynamic(self.slist(l), STy(r.node().bits())),
            TyKind::Closure(d, l) => K::Closure(self.sdef(d), self.slist(l)),
            TyKind::Coroutine(d, l) => K::Coroutine(self.sdef(d), self.slist(l)),
            TyKind::Tuple(l) => K::Tuple(self.slist(l)),
            TyKind::Record(s, l) => K::Record(s, self.slist(l)),
            TyKind::Union(l) => K::Union(self.slist(l)),
            TyKind::Alias { kind, rigid, def, args } => K::Alias(
                if rigid { Rigid::Yes } else { Rigid::No },
                s_alias(kind),
                self.sdef(def),
                self.slist(args),
            ),
            TyKind::Param(i) => K::Param(i),
            TyKind::Bound(d, v) => K::Bound(d, v),
            TyKind::Canonical(v) => K::Canonical(v),
            TyKind::Placeholder(u, v) => K::Placeholder(u.0, v),
            TyKind::TyVar(v) => svar(VarKind::Ty, v),
            TyKind::IntVar(v) => svar(VarKind::Int, v),
            TyKind::FloatVar(v) => svar(VarKind::Float, v),
            TyKind::Error => K::Error,
            TyKind::NotAType => match self.cx.tys.as_pred(t).map(|p| self.cx.tys.pred_kind(p)) {
                Some(PredKind::ExTrait(d, l)) => K::ExTrait(self.sdef(d), self.slist(l)),
                Some(PredKind::ExProjection(d, l)) => {
                    let l = self.slist(l);
                    K::ExProj(self.sdef(d), Self::init(l), self.last(l))
                }
                Some(PredKind::ExAuto(d)) => K::ExAuto(self.sdef(d)),
                // A predicate is never a term the solver is handed: its goals are `Pred` rows.
                p => panic!("blaze-typeck: the solver read predicate {p:?} as a term"),
            },
            // Typeck makes only async blocks' coroutines (`TyView::Future`); an async closure is
            // a closure whose output is one, and the witness is the coroutine's interior tuple.
            k @ (TyKind::CoroutineClosure(..) | TyKind::CoroutineWitness(..)) => {
                panic!("blaze-typeck: the solver has no term for {k:?}")
            }
        }
    }

    /// Relates and hands back the alias pairs relating deferred.
    fn relate(
        &mut self,
        a: STy,
        b: STy,
        how: Relate,
        deferred: &mut Vec<(STy, STy), Alloc<'_>>,
    ) -> Result<bool, SolveError> {
        let (Some(x), Some(y)) = (self.node(a)?.arg(), self.node(b)?.arg()) else {
            return Err(sort(a.0, Met::List));
        };
        let got = match how {
            Relate::Lazy => self.cx.eq(x, y, &mut AllInvariant),
            Relate::Structural => self.cx.unify_structurally(x, y, &mut AllInvariant),
        };
        self.goals.clear();
        self.cx.take_goals(&mut self.goals);
        self.cx.clear_outlives();
        match got {
            Ok(_) => {
                for &g in self.goals.iter() {
                    match self.cx.tys.pred_kind(g) {
                        PredKind::Projection(alias, term) => {
                            deferred.push((STy(alias.node().bits()), STy(term.node().bits())));
                        }
                        // Relating invariantly emits only projections; a subtype goal would
                        // need a covariant relation, which the solver never asks of `unify`.
                        p => panic!("blaze-typeck: relating emitted {p:?}"),
                    }
                }
                Ok(true)
            }
            Err(_) => Ok(false),
        }
    }
}

impl<'a> blaze_solve::Tys for Terms<'a> {
    type Snapshot = blaze_ty::Snapshot;
    const REGIONS_BIND: bool = false;

    fn kind(&self, t: STy) -> K {
        let Some(n) = self.cx.tys.node_at(t.0) else { panic!("blaze-typeck: term {} is not in the arena", t.0) };
        if let Some(t) = n.as_ty() {
            return self.ty_kind(t);
        }
        if let Some(r) = n.as_region() {
            return match self.cx.tys.region_kind(r) {
                RegionKind::EarlyParam(i) => K::ReParam(i),
                RegionKind::Bound(d, v) => K::Bound(d, v),
                RegionKind::Static => K::ReStatic,
                RegionKind::Var(v) => svar(VarKind::Region, v),
                RegionKind::Placeholder(u, v) => K::Placeholder(u.0, v),
                // Regions are erased in the solver (solve.md, "Not ported").
                RegionKind::LateParam(..) | RegionKind::Erased => K::ReErased,
                RegionKind::Error => K::Error,
                RegionKind::Canonical(v) => K::Canonical(v),
            };
        }
        if let Some(c) = n.as_const() {
            return match self.cx.tys.const_kind(c) {
                ConstKind::Param(i) => K::ConstParam(i),
                ConstKind::Var(v) => svar(VarKind::Const, v),
                ConstKind::Bound(d, v) => K::Bound(d, v),
                ConstKind::Placeholder(u, v) => K::Placeholder(u.0, v),
                ConstKind::Value(t, bits) => match (s_val_ty(self.cx.tys.ty_kind(t)), self.cx.tys.ty_kind(t)) {
                    (Some(vt), _) => K::ConstValue(vt, bits),
                    (None, TyKind::Adt(d, _)) => K::ConstValue(s::ValTy::Enum(self.sdef(d)), bits),
                    // `InferCtx::mk` refuses a value of any other type (`Malformed::ConstValueType`).
                    (None, _) => panic!("blaze-typeck: a const value of a type no const generic has"),
                },
                ConstKind::Unevaluated(d, l) => K::Alias(Rigid::No, SAlias::Const, self.sdef(d), self.slist(l)),
                ConstKind::Error => K::Error,
                ConstKind::Canonical(v) => K::Canonical(v),
            };
        }
        panic!("blaze-typeck: list {} read as a term", t.0)
    }

    fn flags(&self, t: STy) -> SFlags {
        let Some(n) = self.cx.tys.node_at(t.0) else { return SFlags(0) };
        let f = self.cx.tys.node_flags(n);
        let map = [
            (blaze_ty::Flags::TY_INFER | blaze_ty::Flags::CT_INFER, SFlags::HAS_INFER),
            (blaze_ty::Flags::RE_INFER, SFlags::HAS_RE_INFER),
            (blaze_ty::Flags::PARAM, SFlags::HAS_PARAM),
            (blaze_ty::Flags::PLACEHOLDER, SFlags::HAS_PLACEHOLDER),
            (blaze_ty::Flags::ALIAS, SFlags::HAS_ALIAS),
            (blaze_ty::Flags::OPAQUE, SFlags::HAS_OPAQUE),
            (blaze_ty::Flags::ERROR, SFlags::HAS_ERROR),
            (blaze_ty::Flags::FREE_REGIONS, SFlags::HAS_FREE_REGION),
            (blaze_ty::Flags::CANONICAL, SFlags::HAS_CANONICAL),
            (blaze_ty::Flags::NEEDS_NORMALIZE, SFlags::HAS_NON_RIGID_ALIAS),
        ];
        let mut out = 0u16;
        for (b, s) in map {
            if f.has(b) {
                out |= s;
            }
        }
        if f.outer_binder() > 0 {
            out |= SFlags::HAS_BOUND;
        }
        SFlags(out)
    }

    fn shift_in(&mut self, _: &mut blaze_solve::fold::Scratch<'_>, t: STy, by: u32) -> Result<STy, SolveError> {
        let n = self.node(t)?;
        Ok(STy(self.cx.shift_in(n, by).bits()))
    }

    fn reindex_bound(&mut self, original: STy, depth: u32, var: u32) -> Result<STy, SolveError> {
        let n = self.node(original)?;
        let shifted = match met(n) {
            Met::Ty => self.cx.tys.bound(depth, var).node(),
            Met::Region => self.cx.tys.re_bound(depth, var).node(),
            Met::Const => self.cx.tys.const_bound(depth, var).node(),
            other => return Err(sort(original.0, other)),
        };
        Ok(STy(shifted.bits()))
    }

    /// The solver's own encoding, kept verbatim in blaze-ty's marker column (the solver is
    /// its only reader).
    fn markers(&self, t: STy) -> Markers {
        match self.cx.tys.node_at(t.0).and_then(Node::as_ty) {
            Some(ty) => Markers(self.cx.tys.markers(ty) as u8),
            None => Markers(0),
        }
    }

    fn set_markers(&mut self, t: STy, m: Markers) {
        if let Some(ty) = self.cx.tys.node_at(t.0).and_then(Node::as_ty) {
            self.cx.tys.set_markers(ty, u32::from(m.0));
        }
    }

    fn list_get(&self, l: SList, i: u32) -> STy {
        match self.cx.tys.pool_get(l.start + i) {
            Some(a) if i < l.len => STy(a.bits()),
            _ => panic!("blaze-typeck: element {i} of a list of {}", l.len),
        }
    }

    fn mk(&mut self, k: K) -> Result<STy, SolveError> {
        let n: Node = match k {
            K::Bool => Ty::BOOL.node(),
            K::Char => Ty::CHAR.node(),
            K::Int(i) => Ty::int(b_int(i)).node(),
            K::Uint(u) => Ty::uint(b_uint(u)).node(),
            K::Float(f) => Ty::float(b_float(f)).node(),
            K::Str => Ty::STR.node(),
            K::Never => Ty::NEVER.node(),
            K::Tuple(l) => {
                let l = self.blist(l)?;
                self.cx.tys.tuple(l).node()
            }
            K::Record(s, l) => {
                let l = self.blist(l)?;
                self.cx.tys.record(s, l).node()
            }
            K::Union(l) => {
                let l = self.blist(l)?;
                self.cx.tys.union(l).node()
            }
            K::Ref(r, p, m) => {
                let (r, p) = (self.region(r)?, self.ty(p)?);
                self.cx.tys.reference(r, p, b_mut(m)).node()
            }
            K::RawPtr(p, m) => {
                let p = self.ty(p)?;
                self.cx.tys.ptr(p, b_mut(m)).node()
            }
            K::Array(e, n) => {
                let (e, n) = (self.ty(e)?, self.konst(n)?);
                self.cx.tys.array(e, n).node()
            }
            K::Slice(e) => {
                let e = self.ty(e)?;
                self.cx.tys.slice(e).node()
            }
            K::Pat(b, l) => {
                let (b, l) = (self.ty(b)?, self.blist(l)?);
                self.cx.tys.pat(b, l).node()
            }
            K::Adt(d, l) => {
                let (d, l) = (self.bdef(d), self.blist(l)?);
                self.cx.tys.adt(d, l).node()
            }
            K::Foreign(d) => {
                let d = self.bdef(d);
                self.cx.tys.foreign(d).node()
            }
            K::FnDef(d, l) => {
                let (d, l) = (self.bdef(d), self.blist(l)?);
                self.cx.tys.fn_def(d, l).node()
            }
            K::FnPtr(l, h) => {
                let l = self.blist(l)?;
                self.cx.tys.fn_ptr(l, b_header(h)).node()
            }
            K::Closure(d, l) => {
                let (d, l) = (self.bdef(d), self.blist(l)?);
                self.cx.tys.closure(d, l).node()
            }
            K::Coroutine(d, l) => {
                let (d, l) = (self.bdef(d), self.blist(l)?);
                self.cx.tys.coroutine(d, l).node()
            }
            K::Dynamic(l, r) => {
                let (l, r) = (self.blist(l)?, self.region(r)?);
                self.cx.tys.dynamic(l, r).node()
            }
            K::Alias(_, SAlias::Const, d, l) => {
                let (d, l) = (self.bdef(d), self.blist(l)?);
                self.cx.tys.const_unevaluated(d, l).node()
            }
            K::Alias(r, a, d, l) => {
                let kind = match a {
                    SAlias::Projection => AliasKind::Projection,
                    SAlias::Inherent => AliasKind::Inherent,
                    SAlias::Opaque => AliasKind::Opaque,
                    SAlias::Free => AliasKind::Free,
                    SAlias::Const => unreachable!("matched above"),
                };
                let (d, l) = (self.bdef(d), self.blist(l)?);
                match r {
                    Rigid::Yes => self.cx.tys.rigid_alias(kind, d, l).node(),
                    Rigid::No => self.cx.tys.alias(kind, d, l).node(),
                }
            }
            K::Param(i) => self.cx.tys.param(i).node(),
            K::Placeholder(u, v) => {
                // The solver enters universe `u` to make its placeholders (`placeholders`, its
                // `enter_forall`); the arena's current universe follows, so variables made from
                // here on may name them (`InferCtxt::universe` only grows: the reference's
                // `create_next_universe`). A snapshot rolls it back with them.
                // Bounded by `u`: each step raises the universe by one.
                while self.cx.vars.universe().0 < u {
                    self.cx.vars.create_next_universe();
                }
                self.cx.tys.placeholder(Universe(u), v).node()
            }
            K::Bound(d, v) => self.cx.tys.bound(d, v).node(),
            // The solver instantiates canonical forms with fresh variables; it never makes one.
            K::Canonical(v) => return Err(sort(v, Met::Canonical)),
            K::Infer(v) => {
                let id = var_of_index(v.index).ok_or(sort(v.index, Met::Nothing))?;
                self.cx.var_node(id).ok_or(sort(v.index, Met::Nothing))?
            }
            K::Error => Ty::ERROR.node(),
            K::ReStatic => Region::STATIC.node(),
            K::ReErased => Region::ERASED.node(),
            K::ReParam(i) => self.cx.tys.re_early_param(i).node(),
            K::ConstValue(s::ValTy::Enum(d), v) => {
                let adt = self.bdef(d);
                let t = self.cx.tys.adt(adt, List::EMPTY);
                self.cx.tys.const_value(t, v).node()
            }
            K::ConstValue(t, v) => self.cx.tys.const_value(b_val_ty(t), v).node(),
            K::ConstParam(i) => self.cx.tys.const_param(i).node(),
            K::ExTrait(d, l) => {
                let (d, l) = (self.bdef(d), self.blist(l)?);
                self.cx.tys.ex_trait(d, l).node()
            }
            K::ExProj(d, l, t) => {
                let d = self.bdef(d);
                let term = self.node(t)?.arg().ok_or(sort(t.0, Met::List))?;
                self.args.clear();
                for i in 0..l.len {
                    let a = self.cx.tys.pool_get(l.start + i).ok_or(sort(l.start + i, Met::Nothing))?;
                    self.args.push(a);
                }
                self.args.push(term);
                let l = self.cx.tys.mk_list(&self.args);
                self.cx.tys.ex_projection(d, l).node()
            }
            K::ExAuto(d) => {
                let d = self.bdef(d);
                self.cx.tys.ex_auto(d).node()
            }
        };
        Ok(STy(n.bits()))
    }

    fn mk_list(&mut self, items: &[STy]) -> Result<SList, SolveError> {
        self.args.clear();
        for &t in items {
            let a = self.node(t)?.arg().ok_or(sort(t.0, Met::List))?;
            self.args.push(a);
        }
        let l = self.cx.tys.mk_list(&self.args);
        Ok(self.slist(l))
    }

    fn fresh(&mut self, kind: SVarKind) -> Result<STy, SolveError> {
        let n = match kind {
            SVarKind::Ty => self.cx.next_ty_var().node(),
            SVarKind::Int => self.cx.next_int_var().node(),
            SVarKind::Float => self.cx.next_float_var().node(),
            SVarKind::Region => self.cx.next_region_var().node(),
            SVarKind::Const => self.cx.next_const_var().node(),
        };
        Ok(STy(n.bits()))
    }

    fn shallow_resolve(&self, t: STy) -> STy {
        match self.cx.tys.node_at(t.0) {
            Some(n) => match (n.as_ty(), n.as_const()) {
                (Some(ty), _) => STy(self.cx.probe(ty).node().bits()),
                (_, Some(c)) => STy(self.cx.probe_const(c).node().bits()),
                _ => t,
            },
            None => t,
        }
    }

    fn unify(&mut self, a: STy, b: STy, how: Relate, deferred: &mut Vec<(STy, STy), Alloc<'_>>) -> Result<bool, SolveError> {
        self.relate(a, b, how, deferred)
    }

    fn snapshot(&mut self) -> blaze_ty::Snapshot {
        self.cx.snapshot()
    }

    fn rollback_to(&mut self, s: blaze_ty::Snapshot) {
        self.cx.rollback_to(s);
    }

    fn commit(&mut self, s: blaze_ty::Snapshot) {
        self.cx.commit(s);
    }

    fn bindings_len(&self) -> u32 {
        self.cx.vars.bindings().len() as u32
    }

    fn binding(&self, i: u32) -> u32 {
        var_index(self.cx.vars.bindings()[i as usize])
    }

    fn var_count(&self) -> u32 {
        let most = [VarKind::Ty, VarKind::Int, VarKind::Float, VarKind::Region, VarKind::Const]
            .map(|k| self.cx.vars.var_count(k))
            .into_iter()
            .max()
            .unwrap_or(0);
        (most as u32) * 8
    }
}

#[cfg(test)]
#[test]
fn reindexing_a_bound_variable_preserves_its_sort() -> Result<(), SolveError> {
    use blaze_solve::Tys as _;

    let mut terms = Terms::new_in(blaze_own::testing::heap().alloc());
    let old = [
        terms.cx.tys.bound(2, 3).node(),
        terms.cx.tys.re_bound(2, 3).node(),
        terms.cx.tys.const_bound(2, 3).node(),
    ];
    let expected = [
        terms.cx.tys.bound(1, 3).node(),
        terms.cx.tys.re_bound(1, 3).node(),
        terms.cx.tys.const_bound(1, 3).node(),
    ];
    for (var, shifted) in old.into_iter().zip(expected) {
        assert_eq!(terms.reindex_bound(STy(var.bits()), 1, 3)?, STy(shifted.bits()));
    }
    Ok(())
}
