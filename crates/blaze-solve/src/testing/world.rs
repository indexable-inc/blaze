//! The tests' world: the declarations of a case (the prelude's, its file's and its own),
//! lowered into one portable table and the records `Facts` answers with, and the lowering of a
//! case's goals into the arena.

use std::collections::{HashMap, HashSet};
use std::format;
use std::string::{String, ToString};
use std::vec::Vec;

use blaze_own::Alloc;

use super::parse::{BoundA, CaseError, DeclA, Lt, ParamA, Parsed, PredA, TyA};
use crate::assembly::Clause;
use crate::error::SolveError;
use crate::facts::{
    AdtDef, AdtFlags, AssocDef, AssocKind, AssocTy, Facts, Fetch, FnSigDef, ImplAssoc, ImplFlags, ImplHeader, ImplPolarity, ImplSet,
    Missing, OpaqueDef, Portable, Seal, TraitDef, TraitFlags,
};
use crate::prefilter::{self, Columns};
use crate::term::{AliasKind, Def, Lang, List, Polarity, Pred, Rigid, Shape, Ty, TyKind, VarKind};
use crate::tys::Tys;

use super::tys::TestTys;

fn fnv(s: &str, seed: u64) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ seed;
    for b in s.bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h ^ (h >> 31)
}

/// A definition's key from its kind and name: equal names give equal keys in every run.
pub fn def_of(kind: &str, name: &str) -> Def {
    let s = format!("{kind}:{name}");
    Def((u128::from(fnv(&s, 1)) << 64) | u128::from(fnv(&s, 2)))
}

fn lang_of(name: &str) -> Option<Lang> {
    Some(match name {
        "sized" => Lang::Sized,
        "meta_sized" => Lang::MetaSized,
        "pointee_sized" => Lang::PointeeSized,
        "copy" => Lang::Copy,
        "clone" => Lang::Clone,
        "send" => Lang::Send,
        "sync" => Lang::Sync,
        "unpin" => Lang::Unpin,
        "fn" => Lang::Fn,
        "fn_mut" => Lang::FnMut,
        "fn_once" => Lang::FnOnce,
        "fn_once_output" => Lang::FnOnceOutput,
        "fn_ptr_trait" => Lang::FnPtrTrait,
        "tuple" => Lang::Tuple,
        "pointee" => Lang::Pointee,
        "metadata" => Lang::Metadata,
        "dyn_metadata" => Lang::DynMetadata,
        "discriminant_kind" => Lang::DiscriminantKind,
        "discriminant" => Lang::Discriminant,
        "destruct" => Lang::Destruct,
        "drop" => Lang::Drop,
        "unsize" => Lang::Unsize,
        "from" => Lang::From,
        "coerce_unsized" => Lang::CoerceUnsized,
        "dispatch_from_dyn" => Lang::DispatchFromDyn,
        "phantom_data" => Lang::PhantomData,
        "owned_box" => Lang::OwnedBox,
        "manually_drop" => Lang::ManuallyDrop,
        "future_trait" => Lang::Future,
        "future_output" => Lang::FutureOutput,
        "async_fn" => Lang::AsyncFn,
        "async_fn_mut" => Lang::AsyncFnMut,
        "async_fn_once" => Lang::AsyncFnOnce,
        "async_fn_once_output" => Lang::AsyncFnOnceOutput,
        "call_once_future" => Lang::CallOnceFuture,
        "call_ref_future" => Lang::CallRefFuture,
        _ => return None,
    })
}

/// Where lowered terms go: the world's portable table, or a case's arena.
pub trait Sink {
    fn node(&mut self, k: TyKind) -> Result<Ty, SolveError>;
    fn list(&mut self, xs: &[Ty]) -> Result<List, SolveError>;
    fn kind_of(&self, t: Ty) -> TyKind;
    /// The variable named `name` (made on first use), or `None` where variables are not allowed.
    fn var(&mut self, name: &str, kind: VarKind) -> Result<Option<Ty>, SolveError>;
}

/// The portable table: hash-consed nodes and lists every record's terms index.
#[derive(Default)]
pub struct Table {
    pub nodes: Vec<TyKind>,
    map: HashMap<TyKind, Ty>,
    pub lists: Vec<Ty>,
    list_map: HashMap<Vec<Ty>, List>,
}

impl Sink for Table {
    fn node(&mut self, k: TyKind) -> Result<Ty, SolveError> {
        if let Some(&t) = self.map.get(&k) {
            return Ok(t);
        }
        let t = Ty(self.nodes.len() as u32);
        self.nodes.push(k);
        self.map.insert(k, t);
        Ok(t)
    }

    fn list(&mut self, xs: &[Ty]) -> Result<List, SolveError> {
        if xs.is_empty() {
            return Ok(List::EMPTY);
        }
        if let Some(&l) = self.list_map.get(xs) {
            return Ok(l);
        }
        let l = List { start: self.lists.len() as u32, len: xs.len() as u32 };
        self.lists.extend_from_slice(xs);
        self.list_map.insert(xs.to_vec(), l);
        Ok(l)
    }

    fn kind_of(&self, t: Ty) -> TyKind {
        self.nodes[t.0 as usize]
    }

    fn var(&mut self, _: &str, _: VarKind) -> Result<Option<Ty>, SolveError> {
        Ok(None)
    }
}

/// A case's arena with its named variables.
pub struct Arena<'x, T: Tys = TestTys> {
    pub tys: &'x mut T,
    pub vars: &'x mut HashMap<String, Ty>,
}

impl<T: Tys> Sink for Arena<'_, T> {
    fn node(&mut self, k: TyKind) -> Result<Ty, SolveError> {
        self.tys.mk(k)
    }

    fn list(&mut self, xs: &[Ty]) -> Result<List, SolveError> {
        self.tys.mk_list(xs)
    }

    fn kind_of(&self, t: Ty) -> TyKind {
        self.tys.kind(t)
    }

    fn var(&mut self, name: &str, kind: VarKind) -> Result<Option<Ty>, SolveError> {
        if let Some(&t) = self.vars.get(name) {
            return Ok(Some(t));
        }
        let t = self.tys.fresh(kind)?;
        self.vars.insert(name.to_string(), t);
        Ok(Some(t))
    }
}

/// A trait's name entry.
#[derive(Clone, Debug)]
struct TraitName {
    def: Def,
    generics: u32,
    auto: bool,
    supers: Vec<String>,
}

/// The names in scope of a case's declarations.
#[derive(Default)]
pub struct Names {
    traits: HashMap<String, TraitName>,
    assoc: HashMap<(Def, String), (Def, AssocKind)>,
    adts: HashMap<String, Def>,
    foreign: HashMap<String, Def>,
    fns: HashMap<String, Def>,
    opaques: HashMap<String, Def>,
    pub impls: HashMap<String, Def>,
}

/// What names a term's parameters and bound variables resolve to.
#[derive(Clone, Copy)]
pub struct Scope<'s> {
    pub params: &'s [String],
    pub bound: &'s [String],
}

fn err(line: u32, what: impl Into<String>) -> CaseError {
    CaseError { line, what: what.into() }
}

fn solve_err(line: u32, e: SolveError) -> CaseError {
    err(line, format!("{e}"))
}

impl Names {
    fn trait_(&self, line: u32, n: &str) -> Parsed<&TraitName> {
        self.traits.get(n).ok_or_else(|| err(line, format!("no trait `{n}`")))
    }

    /// The associated item `name` of `trait_` or of one of its supertraits (by name).
    fn item(&self, line: u32, trait_: &str, name: &str) -> Parsed<(Def, AssocKind, u32)> {
        let mut work = std::vec![trait_.to_string()];
        let mut seen = HashSet::new();
        while let Some(t) = work.pop() {
            if !seen.insert(t.clone()) {
                continue;
            }
            let tn = self.trait_(line, &t)?;
            if let Some(&(d, k)) = self.assoc.get(&(tn.def, name.to_string())) {
                return Ok((d, k, tn.generics));
            }
            work.extend(tn.supers.iter().cloned());
        }
        Err(err(line, format!("no item `{name}` in `{trait_}` or its supertraits")))
    }

    pub fn ty<'a, S: Sink>(&self, s: &mut S, sc: Scope<'_>, line: u32, t: &'a TyA) -> Parsed<Ty> {
        enum Work<'a> {
            Eval(&'a TyA),
            EvalConstVar(&'a str),
            Tuple(usize),
            Ref(&'a Lt, crate::term::Mutbl),
            Ptr(crate::term::Mutbl),
            Slice,
            Array,
            Adt(Def, usize),
            FnPtr(usize),
            FnDef(Def, usize),
            Closure { n: u64, ups_tuple_len: Option<usize> },
            Dyn { bounds: &'a [BoundA], lt: &'a Lt },
            Proj { item: Def, kind: AssocKind, generics: u32, total_args: usize, own_len: usize, rigid: bool },
            Opaque(Def, bool, usize),
        }
        let e = |x: SolveError| solve_err(line, x);
        let mut work: [Option<Work<'a>>; 256] = [const { None }; 256];
        let mut wp = 0usize;
        let mut vals = [Ty::NONE; 256];
        let mut vp = 0usize;
        let push_w = |work: &mut [Option<Work<'a>>; 256], wp: &mut usize, w: Work<'a>| -> Parsed<()> {
            let Some(slot) = work.get_mut(*wp) else {
                return Err(err(line, "type lowering stack overflow"));
            };
            *slot = Some(w);
            *wp += 1;
            Ok(())
        };
        let push_v = |vals: &mut [Ty; 256], vp: &mut usize, v: Ty| -> Parsed<()> {
            let Some(slot) = vals.get_mut(*vp) else {
                return Err(err(line, "type lowering value stack overflow"));
            };
            *slot = v;
            *vp += 1;
            Ok(())
        };
        let pop_v = |vals: &[Ty; 256], vp: &mut usize| -> Parsed<Ty> {
            *vp = vp.checked_sub(1).ok_or_else(|| err(line, "type lowering value stack underflow"))?;
            vals.get(*vp).copied().ok_or_else(|| err(line, "type lowering value stack out of bounds"))
        };
        fn slice_v(line: u32, vals: &[Ty; 256], start: usize, len: usize) -> Parsed<&[Ty]> {
            let end = start.checked_add(len).ok_or_else(|| err(line, "type lowering slice overflow"))?;
            vals.get(start..end).ok_or_else(|| err(line, "type lowering slice out of bounds"))
        }
        push_w(&mut work, &mut wp, Work::Eval(t))?;
        while wp > 0 {
            wp -= 1;
            let Some(item) = work.get_mut(wp).and_then(Option::take) else { break };
            match item {
                Work::EvalConstVar(v) => {
                    let ty = s
                        .var(v, VarKind::Const)
                        .map_err(e)?
                        .ok_or_else(|| err(line, "a variable outside a case"))?;
                    push_v(&mut vals, &mut vp, ty)?;
                }
                Work::Eval(node) => match node {
                    TyA::Bool => push_v(&mut vals, &mut vp, s.node(TyKind::Bool).map_err(e)?)?,
                    TyA::Char => push_v(&mut vals, &mut vp, s.node(TyKind::Char).map_err(e)?)?,
                    TyA::Int(i) => push_v(&mut vals, &mut vp, s.node(TyKind::Int(*i)).map_err(e)?)?,
                    TyA::Uint(u) => push_v(&mut vals, &mut vp, s.node(TyKind::Uint(*u)).map_err(e)?)?,
                    TyA::Float(f) => push_v(&mut vals, &mut vp, s.node(TyKind::Float(*f)).map_err(e)?)?,
                    TyA::Str => push_v(&mut vals, &mut vp, s.node(TyKind::Str).map_err(e)?)?,
                    TyA::Never => push_v(&mut vals, &mut vp, s.node(TyKind::Never).map_err(e)?)?,
                    TyA::Error => push_v(&mut vals, &mut vp, s.node(TyKind::Error).map_err(e)?)?,
                    TyA::Const(n) | TyA::Kind(n) => {
                        push_v(&mut vals, &mut vp, s.node(TyKind::ConstValue(crate::term::ValTy::USIZE, u128::from(*n))).map_err(e)?)?;
                    }
                    TyA::Placeholder(i) => push_v(&mut vals, &mut vp, s.node(TyKind::Placeholder(1, *i)).map_err(e)?)?,
                    TyA::Region(lt) => push_v(&mut vals, &mut vp, self.region(s, line, lt)?)?,
                    TyA::Tuple(xs) => {
                        push_w(&mut work, &mut wp, Work::Tuple(xs.len()))?;
                        for x in xs.iter().rev() {
                            push_w(&mut work, &mut wp, Work::Eval(x))?;
                        }
                    }
                    TyA::Ref(lt, x, m) => {
                        push_w(&mut work, &mut wp, Work::Ref(lt, *m))?;
                        push_w(&mut work, &mut wp, Work::Eval(x))?;
                    }
                    TyA::Ptr(x, m) => {
                        push_w(&mut work, &mut wp, Work::Ptr(*m))?;
                        push_w(&mut work, &mut wp, Work::Eval(x))?;
                    }
                    TyA::Slice(x) => {
                        push_w(&mut work, &mut wp, Work::Slice)?;
                        push_w(&mut work, &mut wp, Work::Eval(x))?;
                    }
                    TyA::Array(x, n) => {
                        push_w(&mut work, &mut wp, Work::Array)?;
                        push_w(&mut work, &mut wp, Work::Eval(n))?;
                        push_w(&mut work, &mut wp, Work::Eval(x))?;
                    }
                    TyA::Path(name, args) => {
                        if args.is_empty() {
                            if let Some(i) = sc.bound.iter().position(|b| b == name) {
                                let idx = u32::try_from(i).map_err(|_| err(line, "bound index overflow"))?;
                                push_v(&mut vals, &mut vp, s.node(TyKind::Bound(0, idx)).map_err(e)?)?;
                                continue;
                            }
                            if let Some(i) = sc.params.iter().position(|b| b == name) {
                                let idx = u32::try_from(i).map_err(|_| err(line, "param index overflow"))?;
                                push_v(&mut vals, &mut vp, s.node(TyKind::Param(idx)).map_err(e)?)?;
                                continue;
                            }
                        }
                        if let Some(&d) = self.adts.get(name) {
                            push_w(&mut work, &mut wp, Work::Adt(d, args.len()))?;
                            for a in args.iter().rev() {
                                push_w(&mut work, &mut wp, Work::Eval(a))?;
                            }
                        } else if let Some(&d) = self.foreign.get(name) {
                            push_v(&mut vals, &mut vp, s.node(TyKind::Foreign(d)).map_err(e)?)?;
                        } else {
                            return Err(err(line, format!("no type `{name}`")));
                        }
                    }
                    TyA::FnPtr(ins, out) => {
                        push_w(&mut work, &mut wp, Work::FnPtr(ins.len() + 1))?;
                        push_w(&mut work, &mut wp, Work::Eval(out))?;
                        for x in ins.iter().rev() {
                            push_w(&mut work, &mut wp, Work::Eval(x))?;
                        }
                    }
                    TyA::FnItem(name, args) => {
                        let d = *self.fns.get(name).ok_or_else(|| err(line, format!("no fn `{name}`")))?;
                        push_w(&mut work, &mut wp, Work::FnDef(d, args.len()))?;
                        for a in args.iter().rev() {
                            push_w(&mut work, &mut wp, Work::Eval(a))?;
                        }
                    }
                    TyA::Closure(n, kind, sig, ups) => match &**ups {
                        TyA::Tuple(xs) => {
                            push_w(&mut work, &mut wp, Work::Closure { n: *n, ups_tuple_len: Some(xs.len()) })?;
                            for x in xs.iter().rev() {
                                push_w(&mut work, &mut wp, Work::Eval(x))?;
                            }
                            push_w(&mut work, &mut wp, Work::Eval(sig))?;
                            match &**kind {
                                TyA::Var(v) => push_w(&mut work, &mut wp, Work::EvalConstVar(v))?,
                                other => push_w(&mut work, &mut wp, Work::Eval(other))?,
                            }
                        }
                        other => {
                            push_w(&mut work, &mut wp, Work::Closure { n: *n, ups_tuple_len: None })?;
                            push_w(&mut work, &mut wp, Work::Eval(other))?;
                            push_w(&mut work, &mut wp, Work::Eval(sig))?;
                            match &**kind {
                                TyA::Var(v) => push_w(&mut work, &mut wp, Work::EvalConstVar(v))?,
                                k => push_w(&mut work, &mut wp, Work::Eval(k))?,
                            }
                        }
                    },
                    TyA::Dyn(bounds, lt) => {
                        push_w(&mut work, &mut wp, Work::Dyn { bounds, lt })?;
                        for b in bounds.iter().rev() {
                            if !b.assoc_bounds.is_empty() {
                                return Err(err(line, "an associated item constraint with bounds in a `dyn` type"));
                            }
                            let tn = self.trait_(line, &b.trait_)?;
                            if tn.auto {
                                continue;
                            }
                            for (_, v) in b.assoc.iter().rev() {
                                push_w(&mut work, &mut wp, Work::Eval(v))?;
                            }
                            for a in b.args.iter().rev() {
                                push_w(&mut work, &mut wp, Work::Eval(a))?;
                            }
                        }
                    }
                    TyA::Proj { self_ty, trait_, args, name, own, rigid } => {
                        let (item, kind, generics) = self.item(line, trait_, name)?;
                        push_w(
                            &mut work,
                            &mut wp,
                            Work::Proj { item, kind, generics, total_args: 1 + args.len(), own_len: own.len(), rigid: *rigid },
                        )?;
                        for a in own.iter().rev() {
                            push_w(&mut work, &mut wp, Work::Eval(a))?;
                        }
                        for a in args.iter().rev() {
                            push_w(&mut work, &mut wp, Work::Eval(a))?;
                        }
                        push_w(&mut work, &mut wp, Work::Eval(self_ty))?;
                    }
                    TyA::Opaque(name, args, rigid) => {
                        let d = *self.opaques.get(name).ok_or_else(|| err(line, format!("no opaque `{name}`")))?;
                        push_w(&mut work, &mut wp, Work::Opaque(d, *rigid, args.len()))?;
                        for a in args.iter().rev() {
                            push_w(&mut work, &mut wp, Work::Eval(a))?;
                        }
                    }
                    TyA::Var(name) => {
                        let kind = match name.as_bytes() {
                            [b'i', rest @ ..] if !rest.is_empty() && rest.iter().all(u8::is_ascii_digit) => VarKind::Int,
                            [b'f', rest @ ..] if !rest.is_empty() && rest.iter().all(u8::is_ascii_digit) => VarKind::Float,
                            _ => VarKind::Ty,
                        };
                        let ty = s
                            .var(name, kind)
                            .map_err(e)?
                            .ok_or_else(|| err(line, format!("variable ?{name} outside a case")))?;
                        push_v(&mut vals, &mut vp, ty)?;
                    }
                },
                Work::Tuple(n) => {
                    vp = vp.saturating_sub(n);
                    let l = s.list(slice_v(line, &vals, vp, n)?).map_err(e)?;
                    push_v(&mut vals, &mut vp, s.node(TyKind::Tuple(l)).map_err(e)?)?;
                }
                Work::Ref(lt, m) => {
                    let x = pop_v(&vals, &mut vp)?;
                    let r = self.region(s, line, lt)?;
                    push_v(&mut vals, &mut vp, s.node(TyKind::Ref(r, x, m)).map_err(e)?)?;
                }
                Work::Ptr(m) => {
                    let x = pop_v(&vals, &mut vp)?;
                    push_v(&mut vals, &mut vp, s.node(TyKind::RawPtr(x, m)).map_err(e)?)?;
                }
                Work::Slice => {
                    let x = pop_v(&vals, &mut vp)?;
                    push_v(&mut vals, &mut vp, s.node(TyKind::Slice(x)).map_err(e)?)?;
                }
                Work::Array => {
                    let n = pop_v(&vals, &mut vp)?;
                    let x = pop_v(&vals, &mut vp)?;
                    push_v(&mut vals, &mut vp, s.node(TyKind::Array(x, n)).map_err(e)?)?;
                }
                Work::Adt(d, n) => {
                    vp = vp.saturating_sub(n);
                    let l = s.list(slice_v(line, &vals, vp, n)?).map_err(e)?;
                    push_v(&mut vals, &mut vp, s.node(TyKind::Adt(d, l)).map_err(e)?)?;
                }
                Work::FnPtr(n) => {
                    vp = vp.saturating_sub(n);
                    let l = s.list(slice_v(line, &vals, vp, n)?).map_err(e)?;
                    push_v(&mut vals, &mut vp, s.node(TyKind::FnPtr(l, crate::term::FnPtrHeader::RUST)).map_err(e)?)?;
                }
                Work::FnDef(d, n) => {
                    vp = vp.saturating_sub(n);
                    let l = s.list(slice_v(line, &vals, vp, n)?).map_err(e)?;
                    push_v(&mut vals, &mut vp, s.node(TyKind::FnDef(d, l)).map_err(e)?)?;
                }
                Work::Closure { n, ups_tuple_len } => {
                    let ups = match ups_tuple_len {
                        Some(ulen) => {
                            vp = vp.saturating_sub(ulen);
                            let l = s.list(slice_v(line, &vals, vp, ulen)?).map_err(e)?;
                            s.node(TyKind::Tuple(l)).map_err(e)?
                        }
                        None => {
                            let one = pop_v(&vals, &mut vp)?;
                            let l = s.list(&[one]).map_err(e)?;
                            s.node(TyKind::Tuple(l)).map_err(e)?
                        }
                    };
                    let sig = pop_v(&vals, &mut vp)?;
                    let k = pop_v(&vals, &mut vp)?;
                    let l = s.list(&[k, sig, ups]).map_err(e)?;
                    push_v(&mut vals, &mut vp, s.node(TyKind::Closure(def_of("closure", &n.to_string()), l)).map_err(e)?)?;
                }
                Work::Dyn { bounds, lt } => {
                    let mut total = 0usize;
                    for b in bounds {
                        let tn = self.trait_(line, &b.trait_)?;
                        if !tn.auto {
                            total += b.args.len() + b.assoc.len();
                        }
                    }
                    vp = vp.saturating_sub(total);
                    let mut cursor = vp;
                    let mut principal = Vec::new();
                    let mut projs = Vec::new();
                    let mut autos = Vec::new();
                    for b in bounds {
                        let tn = self.trait_(line, &b.trait_)?.clone();
                        if tn.auto {
                            autos.push(s.node(TyKind::ExAuto(tn.def)).map_err(e)?);
                            continue;
                        }
                        let args = s.list(slice_v(line, &vals, cursor, b.args.len())?).map_err(e)?;
                        cursor += b.args.len();
                        principal.push(s.node(TyKind::ExTrait(tn.def, args)).map_err(e)?);
                        for (name, _) in &b.assoc {
                            let (item, _, _) = self.item(line, &b.trait_, name)?;
                            let v = vals.get(cursor).copied().ok_or_else(|| err(line, "dyn value stack out of bounds"))?;
                            cursor += 1;
                            projs.push((item, s.node(TyKind::ExProj(item, args, v)).map_err(e)?));
                        }
                    }
                    if principal.len() > 1 {
                        return Err(err(line, "a dyn type with two principal traits"));
                    }
                    projs.sort_by_key(|p| p.0);
                    let mut autos_d: Vec<(Def, Ty)> = autos
                        .into_iter()
                        .map(|t| match s.kind_of(t) {
                            TyKind::ExAuto(d) => (d, t),
                            _ => (Def(0), t),
                        })
                        .collect();
                    autos_d.sort_by_key(|p| p.0);
                    autos_d.dedup_by_key(|p| p.0);
                    let mut all = principal;
                    all.extend(projs.into_iter().map(|p| p.1));
                    all.extend(autos_d.into_iter().map(|p| p.1));
                    let l = s.list(&all).map_err(e)?;
                    let r = self.region(s, line, lt)?;
                    push_v(&mut vals, &mut vp, s.node(TyKind::Dynamic(l, r)).map_err(e)?)?;
                }
                Work::Proj { item, kind, generics, total_args, own_len, rigid } => {
                    vp = vp.saturating_sub(total_args + own_len);
                    let keep = total_args.min(usize::try_from(generics).unwrap_or(total_args));
                    let mut buf = [Ty::NONE; 64];
                    let Some(first_dst) = buf.get_mut(..keep) else {
                        return Err(err(line, "too many projection arguments"));
                    };
                    first_dst.copy_from_slice(slice_v(line, &vals, vp, keep)?);
                    let Some(own_dst) = buf.get_mut(keep..keep + own_len) else {
                        return Err(err(line, "too many projection arguments"));
                    };
                    own_dst.copy_from_slice(slice_v(line, &vals, vp + total_args, own_len)?);
                    let all_slice = buf.get(..keep + own_len).ok_or_else(|| err(line, "too many projection arguments"))?;
                    let l = s.list(all_slice).map_err(e)?;
                    let ak = if kind == AssocKind::Const { AliasKind::Const } else { AliasKind::Projection };
                    push_v(&mut vals, &mut vp, s.node(TyKind::Alias(if rigid { Rigid::Yes } else { Rigid::No }, ak, item, l)).map_err(e)?)?;
                }
                Work::Opaque(d, rigid, n) => {
                    vp = vp.saturating_sub(n);
                    let l = s.list(slice_v(line, &vals, vp, n)?).map_err(e)?;
                    push_v(&mut vals, &mut vp, s.node(TyKind::Alias(if rigid { Rigid::Yes } else { Rigid::No }, AliasKind::Opaque, d, l)).map_err(e)?)?;
                }
            }
        }
        vals.first().copied().ok_or_else(|| err(line, "empty type lowering stack".to_string()))
    }

    fn region<S: Sink>(&self, s: &mut S, line: u32, lt: &Lt) -> Parsed<Ty> {
        let k = match lt {
            Lt::Static => TyKind::ReStatic,
            Lt::Erased => TyKind::ReErased,
            Lt::Named(n) => TyKind::ReParam(fnv(n, 3) as u32 & 0xffff),
        };
        s.node(k).map_err(|x| solve_err(line, x))
    }

    fn tys<S: Sink>(&self, s: &mut S, sc: Scope<'_>, line: u32, xs: &[TyA]) -> Parsed<List> {
        let mut v = Vec::new();
        for x in xs {
            v.push(self.ty(s, sc, line, x)?);
        }
        s.list(&v).map_err(|x| solve_err(line, x))
    }

    /// `self_ty: b` as predicates: the trait predicate, then one projection per binding.
    pub fn bound<S: Sink>(&self, s: &mut S, sc: Scope<'_>, line: u32, self_ty: Ty, b: &BoundA, out: &mut Vec<Pred>) -> Parsed<()> {
        let e = |x: SolveError| solve_err(line, x);
        let mut stack = [(self_ty, b); 64];
        let mut sp = 1usize;
        while sp > 0 {
            sp -= 1;
            let Some(&(self_ty, b)) = stack.get(sp) else { break };
            if b.maybe {
                continue;
            }
            let tn = self.trait_(line, &b.trait_)?.clone();
            let mut v = std::vec![self_ty];
            for a in &b.args {
                v.push(self.ty(s, sc, line, a)?);
            }
            let args = s.list(&v).map_err(e)?;
            let polarity = if b.negative { Polarity::Negative } else { Polarity::Positive };
            out.push(Pred::Trait { trait_: tn.def, args, polarity });
            for (name, val) in &b.assoc {
                let (item, kind, generics) = self.item(line, &b.trait_, name)?;
                let g = usize::try_from(generics).unwrap_or(v.len());
                let slice = v.get(..g).ok_or_else(|| err(line, "assoc generics out of range".to_string()))?;
                let l = s.list(slice).map_err(e)?;
                let ak = if kind == AssocKind::Const { AliasKind::Const } else { AliasKind::Projection };
                let alias = s.node(TyKind::Alias(Rigid::No, ak, item, l)).map_err(e)?;
                let term = self.ty(s, sc, line, val)?;
                out.push(Pred::Projection { alias, term });
            }
            // `Name: Bounds` (RFC 2289): `self_ty: Tr<Name: B>` is `self_ty: Tr` and
            // `<self_ty as Tr>::Name: B`, nested as written (`Tr<A: U<B: V>>`).
            let base = sp;
            for (name, list) in &b.assoc_bounds {
                let (item, kind, generics) = self.item(line, &b.trait_, name)?;
                if kind == AssocKind::Const {
                    return Err(err(line, format!("`{name}: ..` bounds an associated const")));
                }
                let g = usize::try_from(generics).unwrap_or(v.len());
                let slice = v.get(..g).ok_or_else(|| err(line, "assoc generics out of range".to_string()))?;
                let l = s.list(slice).map_err(e)?;
                let alias = s.node(TyKind::Alias(Rigid::No, AliasKind::Projection, item, l)).map_err(e)?;
                for nb in list {
                    let Some(slot) = stack.get_mut(sp) else {
                        return Err(err(line, "nested associated-type bound depth exceeded".to_string()));
                    };
                    *slot = (alias, nb);
                    sp += 1;
                }
            }
            if let Some(pushed) = stack.get_mut(base..sp) {
                pushed.reverse();
            }
        }
        Ok(())
    }

    /// A predicate as clauses (a bound with bindings gives several), with its binder count.
    pub fn pred<S: Sink>(&self, s: &mut S, sc: Scope<'_>, line: u32, p: &PredA, out: &mut Vec<Clause>) -> Parsed<()> {
        let e = |x: SolveError| solve_err(line, x);
        let (sc, bound, p) = match p {
            PredA::ForAll(names, inner) => (Scope { params: sc.params, bound: names }, names.len() as u32, &**inner),
            other => (sc, 0, other),
        };
        let mut preds = Vec::new();
        match p {
            PredA::Bound(t, b) => {
                let t = self.ty(s, sc, line, t)?;
                self.bound(s, sc, line, t, b, &mut preds)?;
            }
            PredA::Eq(a, b) => {
                let a = self.ty(s, sc, line, a)?;
                let b = self.ty(s, sc, line, b)?;
                if matches!(s.kind_of(a), TyKind::Alias(..)) {
                    preds.push(Pred::Projection { alias: a, term: b });
                } else {
                    preds.push(Pred::Eq { a, b });
                }
            }
            PredA::Normalizes(a, b) => {
                let a = self.ty(s, sc, line, a)?;
                let b = self.ty(s, sc, line, b)?;
                preds.push(Pred::NormalizesTo { alias: a, term: b });
            }
            PredA::Wf(t) => preds.push(Pred::WellFormed(self.ty(s, sc, line, t)?)),
            PredA::Sub(a, b) => {
                let a = self.ty(s, sc, line, a)?;
                let b = self.ty(s, sc, line, b)?;
                preds.push(Pred::Subtype { a, b });
            }
            PredA::Const(name, args) => {
                let mut v = Vec::with_capacity(args.len());
                for a in args {
                    v.push(self.ty(s, sc, line, a)?);
                }
                let args = s.list(&v).map_err(e)?;
                preds.push(Pred::ConstHolds { cond: def_of("const", name), args });
            }
            PredA::ForAll(..) => return Err(err(line, "nested `for<..>`".to_string())),
        }
        let _ = e;
        out.extend(preds.into_iter().map(|pred| Clause { pred, bound }));
        Ok(())
    }

    /// The where-clauses of generic parameters: their bounds, and `P: Sized` unless `?Sized`.
    pub fn param_clauses<S: Sink>(&self, s: &mut S, sc: Scope<'_>, line: u32, first: u32, params: &[ParamA], out: &mut Vec<Pred>) -> Parsed<()> {
        for (i, p) in params.iter().enumerate() {
            let t = s.node(TyKind::Param(first + i as u32)).map_err(|x| solve_err(line, x))?;
            self.sized(s, line, t, &p.bounds, out)?;
            for b in &p.bounds {
                self.bound(s, sc, line, t, b, out)?;
            }
        }
        Ok(())
    }

    /// `t: Sized` unless `bounds` has `?Sized`.
    fn sized<S: Sink>(&self, s: &mut S, line: u32, t: Ty, bounds: &[BoundA], out: &mut Vec<Pred>) -> Parsed<()> {
        if bounds.iter().any(|b| b.maybe) {
            return Ok(());
        }
        let sized = self.trait_(line, "Sized")?.def;
        let args = s.list(&[t]).map_err(|x| solve_err(line, x))?;
        out.push(Pred::Trait { trait_: sized, args, polarity: Polarity::Positive });
        Ok(())
    }

    /// `P: Sized` for every case parameter (a case's params are a fn's).
    pub fn case_sized<T: Tys>(&self, s: &mut Arena<'_, T>, line: u32, unsized_: &[bool], out: &mut Vec<Clause>) -> Parsed<()> {
        let mut preds = Vec::new();
        for (i, &u) in unsized_.iter().enumerate() {
            if u {
                continue;
            }
            let t = s.node(TyKind::Param(i as u32)).map_err(|x| solve_err(line, x))?;
            self.sized(s, line, t, &[], &mut preds)?;
        }
        out.extend(preds.into_iter().map(|pred| Clause { pred, bound: 0 }));
        Ok(())
    }
}

struct TraitRec {
    flags: TraitFlags,
    generics: u32,
    lang: Option<Lang>,
    supers: Vec<Pred>,
    wheres: Vec<Pred>,
    assoc: Vec<AssocTy>,
    item_bounds: Vec<Pred>,
    digest: u64,
}

struct AdtRec {
    flags: AdtFlags,
    generics: u32,
    fields: Vec<Ty>,
    variants: Vec<(u32, u32)>,
    sized_tail: Option<Ty>,
    wheres: Vec<Pred>,
    digest: u64,
}

struct ImplRec {
    generics: u32,
    polarity: ImplPolarity,
    flags: ImplFlags,
    trait_: Def,
    args: List,
    wheres: Vec<Pred>,
    assoc: Vec<ImplAssoc>,
    digest: u64,
}

struct FnRec {
    generics: u32,
    io: List,
    flags: u16,
    digest: u64,
}

struct OpaqueRec {
    generics: u32,
    bounds: Vec<Pred>,
    hidden: Option<Ty>,
    digest: u64,
}

struct Bucket {
    impls: Vec<Def>,
    columns: blaze_own::Vec<u8, Alloc<'static>>,
    stride: u32,
    digest: u64,
}

/// A case's world.
pub struct World {
    pub names: Names,
    table: Table,
    traits: HashMap<Def, TraitRec>,
    adts: HashMap<Def, AdtRec>,
    impls: HashMap<Def, ImplRec>,
    fns: HashMap<Def, FnRec>,
    assocs: HashMap<Def, AssocDef>,
    opaques: HashMap<Def, OpaqueRec>,
    langs: HashMap<Lang, Def>,
    local: HashSet<Def>,
    buckets: HashMap<(Def, Shape), Bucket>,
    /// Facts questions answered (for the cost example).
    pub asked: core::cell::Cell<u64>,
}

fn digest_of(d: &DeclA, salt: Def) -> u64 {
    fnv(&format!("{d:?}"), salt.0 as u64)
}

impl World {
    /// Lowers `decls`, in order (later names may be used by earlier declarations).
    pub fn build(decls: &[(u32, &DeclA)]) -> Parsed<World> {
        let mut names = Names::default();
        let mut impl_n = 0u32;
        let mut impl_defs = Vec::new();
        // Pass 1: names.
        for &(line, d) in decls {
            match d {
                DeclA::Trait { attrs, name, params, supers, items, .. } => {
                    let def = def_of("trait", name);
                    let tn = TraitName {
                        def,
                        generics: 1 + params.len() as u32,
                        auto: attrs.auto,
                        supers: supers.iter().filter(|b| !b.maybe).map(|b| b.trait_.clone()).collect(),
                    };
                    if names.traits.insert(name.clone(), tn).is_some() {
                        return Err(err(line, format!("trait `{name}` declared twice")));
                    }
                    for it in items {
                        let k = if it.is_const { AssocKind::Const } else { AssocKind::Type };
                        names.assoc.insert((def, it.name.clone()), (def_of("assoc", &format!("{name}::{}", it.name)), k));
                    }
                }
                DeclA::Adt { name, .. } => {
                    names.adts.insert(name.clone(), def_of("adt", name));
                }
                DeclA::Foreign { name, .. } => {
                    names.foreign.insert(name.clone(), def_of("foreign", name));
                }
                DeclA::Fn { name, .. } => {
                    names.fns.insert(name.clone(), def_of("fn", name));
                }
                DeclA::Opaque { name, .. } => {
                    names.opaques.insert(name.clone(), def_of("opaque", name));
                }
                DeclA::Impl { label, .. } => {
                    let key = match label {
                        Some(l) => l.clone(),
                        None => format!("#{impl_n}"),
                    };
                    impl_n += 1;
                    let def = def_of("impl", &key);
                    if names.impls.insert(key.clone(), def).is_some() {
                        return Err(err(line, format!("impl label @{key} used twice")));
                    }
                    impl_defs.push(def);
                }
            }
        }
        let mut w = World {
            names,
            table: Table::default(),
            traits: HashMap::new(),
            adts: HashMap::new(),
            impls: HashMap::new(),
            fns: HashMap::new(),
            assocs: HashMap::new(),
            opaques: HashMap::new(),
            langs: HashMap::new(),
            local: HashSet::new(),
            buckets: HashMap::new(),
            asked: core::cell::Cell::new(0),
        };
        let mut impl_i = 0usize;
        for &(line, d) in decls {
            w.lower_decl(line, d, &impl_defs, &mut impl_i)?;
        }
        w.finish();
        Ok(w)
    }

    fn lower_decl(&mut self, line: u32, d: &DeclA, impl_defs: &[Def], impl_i: &mut usize) -> Parsed<()> {
        let names = &self.names;
        let t = &mut self.table;
        let e = |x: SolveError| solve_err(line, x);
        match d {
            DeclA::Trait { attrs, name, params, supers, wheres: written_wheres, items } => {
                let def = names.trait_(line, name)?.def;
                let mut pn: Vec<String> = std::vec!["Self".to_string()];
                pn.extend(params.iter().map(|p| p.name.clone()));
                let sc = Scope { params: &pn, bound: &[] };
                let self_ty = t.node(TyKind::Param(0)).map_err(e)?;
                let mut sup = Vec::new();
                for b in supers {
                    names.bound(t, sc, line, self_ty, b, &mut sup)?;
                }
                let mut wheres = Vec::new();
                names.param_clauses(t, sc, line, 1, params, &mut wheres)?;
                // The `where` list: the trait's where-clauses, not elaborated by `X: Trait` (the
                // Reference, "Traits", "Supertraits": only bounds on `Self` are supertraits;
                // rustc's `explicit_implied_clauses_of` keeps a where-clause only when its
                // bounded type is `Self`). A `where Self: ..` is not written in a case: it would
                // be a supertrait, written `trait T: ..`.
                for p in written_wheres {
                    let mut cl = Vec::new();
                    names.pred(t, sc, line, p, &mut cl)?;
                    for c in cl {
                        if c.bound != 0 {
                            return Err(err(line, "a trait where-clause under `for<..>`".to_string()));
                        }
                        wheres.push(c.pred);
                    }
                }
                let generics = pn.len() as u32;
                let mut assoc = Vec::new();
                let mut item_bounds = Vec::new();
                for it in items {
                    let (idef, kind) = names.assoc[&(def, it.name.clone())];
                    let mut ipn = pn.clone();
                    ipn.extend(it.params.iter().map(|p| p.name.clone()));
                    let isc = Scope { params: &ipn, bound: &[] };
                    let mut a = Vec::new();
                    for i in 0..ipn.len() as u32 {
                        a.push(t.node(TyKind::Param(i)).map_err(e)?);
                    }
                    let al = t.list(&a).map_err(e)?;
                    let ak = if kind == AssocKind::Const { AliasKind::Const } else { AliasKind::Projection };
                    let alias = t.node(TyKind::Alias(Rigid::No, ak, idef, al)).map_err(e)?;
                    let start = item_bounds.len() as u32;
                    if kind == AssocKind::Type {
                        names.sized(t, line, alias, &it.bounds, &mut item_bounds)?;
                        for b in &it.bounds {
                            names.bound(t, isc, line, alias, b, &mut item_bounds)?;
                        }
                    }
                    let default = match &it.default {
                        Some(v) => Some(names.ty(t, isc, line, v)?),
                        None => None,
                    };
                    assoc.push(AssocTy {
                        def: idef,
                        own_generics: it.params.len() as u32,
                        bounds_start: start,
                        bounds_len: item_bounds.len() as u32 - start,
                        default,
                    });
                    self.assocs.insert(
                        idef,
                        AssocDef {
                            container: def,
                            trait_: Some(def),
                            kind,
                            parent_generics: generics,
                            own_generics: it.params.len() as u32,
                            digest: fnv(&format!("{it:?}"), idef.0 as u64),
                        },
                    );
                    if let Some(l) = &it.lang {
                        let l = lang_of(l).ok_or_else(|| err(line, format!("no lang item `{l}`")))?;
                        self.langs.insert(l, idef);
                    }
                }
                let mut flags = if attrs.dyn_undecided { TraitFlags::DYN_UNDECIDED } else { TraitFlags::DYN_COMPATIBLE };
                for (on, bit) in [
                    (attrs.auto, TraitFlags::AUTO),
                    (attrs.marker, TraitFlags::MARKER),
                    (attrs.coinductive, TraitFlags::COINDUCTIVE),
                    (attrs.fundamental, TraitFlags::FUNDAMENTAL),
                    (attrs.const_, TraitFlags::CONST),
                    (attrs.unsafe_, TraitFlags::UNSAFE),
                ] {
                    if on {
                        flags |= bit;
                    }
                }
                let lang = match &attrs.lang {
                    Some(l) => Some(lang_of(l).ok_or_else(|| err(line, format!("no lang item `{l}`")))?),
                    None => None,
                };
                if let Some(l) = lang {
                    self.langs.insert(l, def);
                }
                if !attrs.extern_ {
                    self.local.insert(def);
                }
                self.traits.insert(
                    def,
                    TraitRec {
                        flags: TraitFlags(flags),
                        generics,
                        lang,
                        supers: sup,
                        wheres,
                        assoc,
                        item_bounds,
                        digest: digest_of(d, def),
                    },
                );
            }
            DeclA::Adt { attrs, name, params, is_enum, variants } => {
                let def = names.adts[name];
                let pn: Vec<String> = params.iter().map(|p| p.name.clone()).collect();
                let sc = Scope { params: &pn, bound: &[] };
                let mut fields = Vec::new();
                let mut vs = Vec::new();
                for v in variants {
                    let start = fields.len() as u32;
                    for f in v {
                        fields.push(names.ty(t, sc, line, f)?);
                    }
                    vs.push((start, v.len() as u32));
                }
                let mut wheres = Vec::new();
                names.param_clauses(t, sc, line, 0, params, &mut wheres)?;
                let mut flags = if *is_enum { AdtFlags::ENUM } else { AdtFlags::STRUCT };
                if attrs.fundamental {
                    flags |= AdtFlags::FUNDAMENTAL;
                }
                if attrs.auto_copy_pub {
                    flags |= AdtFlags::AUTO_COPY_PUB;
                }
                if let Some(l) = &attrs.lang {
                    let l = lang_of(l).ok_or_else(|| err(line, format!("no lang item `{l}`")))?;
                    flags |= match l {
                        Lang::OwnedBox => AdtFlags::BOX,
                        Lang::PhantomData => AdtFlags::PHANTOM_DATA,
                        Lang::ManuallyDrop => AdtFlags::MANUALLY_DROP,
                        _ => 0,
                    };
                    self.langs.insert(l, def);
                }
                if !attrs.extern_ {
                    self.local.insert(def);
                }
                let sized_tail = if *is_enum { None } else { fields.last().copied() };
                self.adts.insert(
                    def,
                    AdtRec {
                        flags: AdtFlags(flags),
                        generics: pn.len() as u32,
                        fields,
                        variants: vs,
                        sized_tail,
                        wheres,
                        digest: digest_of(d, def),
                    },
                );
            }
            DeclA::Impl { attrs, params, negative, trait_, args, self_ty, wheres, items, .. } => {
                let def = impl_defs[*impl_i];
                *impl_i += 1;
                let pn: Vec<String> = params.iter().map(|p| p.name.clone()).collect();
                let sc = Scope { params: &pn, bound: &[] };
                let tdef = names.trait_(line, trait_)?.def;
                let mut v = std::vec![names.ty(t, sc, line, self_ty)?];
                for a in args {
                    v.push(names.ty(t, sc, line, a)?);
                }
                let al = t.list(&v).map_err(e)?;
                let mut wc = Vec::new();
                names.param_clauses(t, sc, line, 0, params, &mut wc)?;
                for p in wheres {
                    let mut cl = Vec::new();
                    names.pred(t, sc, line, p, &mut cl)?;
                    for c in cl {
                        if c.bound != 0 {
                            return Err(err(line, "an impl where-clause under `for<..>`".to_string()));
                        }
                        wc.push(c.pred);
                    }
                }
                let mut assoc = Vec::new();
                let mut fl = 0u16;
                for it in items {
                    let (item, _, _) = names.item(line, trait_, &it.name)?;
                    let mut ipn = pn.clone();
                    ipn.extend(it.params.iter().map(|p| p.name.clone()));
                    let isc = Scope { params: &ipn, bound: &[] };
                    let value = names.ty(t, isc, line, &it.value)?;
                    if it.is_default {
                        fl |= ImplFlags::DEFAULT;
                    }
                    assoc.push(ImplAssoc { trait_item: item, value, own_generics: it.params.len() as u32, is_default: it.is_default });
                }
                if attrs.const_ {
                    fl |= ImplFlags::CONST;
                }
                let polarity = if attrs.reservation {
                    ImplPolarity::Reservation
                } else if *negative {
                    ImplPolarity::Negative
                } else {
                    ImplPolarity::Positive
                };
                if !attrs.extern_ {
                    self.local.insert(def);
                }
                self.impls.insert(
                    def,
                    ImplRec {
                        generics: pn.len() as u32,
                        polarity,
                        flags: ImplFlags(fl),
                        trait_: tdef,
                        args: al,
                        wheres: wc,
                        assoc,
                        digest: digest_of(d, def),
                    },
                );
            }
            DeclA::Fn { attrs, name, params, inputs, output } => {
                let def = names.fns[name];
                let pn: Vec<String> = params.iter().map(|p| p.name.clone()).collect();
                let sc = Scope { params: &pn, bound: &[] };
                let mut v = Vec::new();
                for x in inputs {
                    v.push(names.ty(t, sc, line, x)?);
                }
                v.push(names.ty(t, sc, line, output)?);
                let io = t.list(&v).map_err(e)?;
                let mut flags = 0;
                if attrs.unsafe_ {
                    flags |= FnSigDef::UNSAFE;
                }
                if attrs.const_ {
                    flags |= FnSigDef::CONST;
                }
                if !attrs.extern_ {
                    self.local.insert(def);
                }
                self.fns.insert(def, FnRec { generics: pn.len() as u32, io, flags, digest: digest_of(d, def) });
            }
            DeclA::Opaque { name, params, bounds, hidden } => {
                let def = names.opaques[name];
                let pn: Vec<String> = params.iter().map(|p| p.name.clone()).collect();
                let sc = Scope { params: &pn, bound: &[] };
                let mut a = Vec::new();
                for i in 0..pn.len() as u32 {
                    a.push(t.node(TyKind::Param(i)).map_err(e)?);
                }
                let al = t.list(&a).map_err(e)?;
                let self_ty = t.node(TyKind::Alias(Rigid::No, AliasKind::Opaque, def, al)).map_err(e)?;
                let mut bs = Vec::new();
                for b in bounds {
                    names.bound(t, sc, line, self_ty, b, &mut bs)?;
                }
                let hidden = match hidden {
                    Some(h) => Some(names.ty(t, sc, line, h)?),
                    None => None,
                };
                self.local.insert(def);
                self.opaques.insert(def, OpaqueRec { generics: pn.len() as u32, bounds: bs, hidden, digest: digest_of(d, def) });
            }
            DeclA::Foreign { attrs, name } => {
                let def = names.foreign[name];
                if !attrs.extern_ {
                    self.local.insert(def);
                }
            }
        }
        Ok(())
    }

    /// `HAS_DROP` and the impl buckets, once every declaration is lowered.
    fn finish(&mut self) {
        if let Some(&drop) = self.langs.get(&Lang::Drop) {
            for i in self.impls.values() {
                if i.trait_ != drop {
                    continue;
                }
                let self_ty = self.table.lists[i.args.start as usize];
                if let TyKind::Adt(d, _) = self.table.nodes[self_ty.0 as usize] {
                    if let Some(a) = self.adts.get_mut(&d) {
                        a.flags.0 |= AdtFlags::HAS_DROP;
                    }
                }
            }
        }
        // Impls in a fixed order (by key), then bucketed per trait and shape.
        let mut order: Vec<Def> = self.impls.keys().copied().collect();
        order.sort();
        let mut by_trait: HashMap<Def, Vec<(Def, Shape)>> = HashMap::new();
        for d in order {
            let i = &self.impls[&d];
            let self_ty = self.table.lists[i.args.start as usize];
            // Filed as the solver asks (object_shape.rs): an object by its principal.
            let (nodes, lists) = (&self.table.nodes, &self.table.lists);
            let first = |l: List| if l.len > 0 { Some(nodes[lists[l.start as usize].0 as usize]) } else { None };
            let shape = crate::object_shape::shape_with_principal(nodes[self_ty.0 as usize], first).unwrap_or(Shape::Blanket);
            by_trait.entry(i.trait_).or_default().push((d, shape));
        }
        for (tr, impls) in by_trait {
            let mut shapes: Vec<Shape> = impls.iter().map(|p| p.1).collect();
            shapes.extend([Shape::Blanket, Shape::AnyInt, Shape::AnyFloat]);
            shapes.sort();
            shapes.dedup();
            for s in shapes {
                let wanted = |x: Shape| -> bool {
                    match s {
                        Shape::Blanket => false,
                        Shape::AnyInt => matches!(x, Shape::Int(_) | Shape::Uint(_)),
                        Shape::AnyFloat => matches!(x, Shape::Float(_)),
                        _ => x == s,
                    }
                };
                let mut list: Vec<Def> = impls.iter().filter(|p| wanted(p.1)).map(|p| p.0).collect();
                list.extend(impls.iter().filter(|p| p.1 == Shape::Blanket).map(|p| p.0));
                let portable = Portable { nodes: &self.table.nodes, lists: &self.table.lists };
                let fps: Vec<prefilter::Fingerprint> =
                    list.iter().map(|d| prefilter::fingerprint_impl(portable, self.impls[d].args)).collect();
                let mut columns = blaze_own::Vec::new_in(blaze_own::testing::heap().alloc());
                let stride = prefilter::columns_of(&fps, &mut columns);
                let mut digest = 0x51u64;
                for d in &list {
                    digest = (digest ^ self.impls[d].digest).wrapping_mul(0x0000_0100_0000_01b3);
                }
                self.buckets.insert((tr, s), Bucket { impls: list, columns, stride, digest });
            }
        }
    }

    pub fn portable(&self) -> Portable<'_> {
        Portable { nodes: &self.table.nodes, lists: &self.table.lists }
    }

    /// Drops `item` from the members `trait_`'s facts hold, keeping its own `assoc` record:
    /// the state of a dependency trait whose members were not filed while a name still
    /// resolves to one of them (tests/member_not_filed.rs). Whether it was there.
    pub fn forget_member(&mut self, trait_: Def, item: Def) -> bool {
        let Some(t) = self.traits.get_mut(&trait_) else { return false };
        let before = t.assoc.len();
        t.assoc.retain(|a| a.def != item);
        t.assoc.len() != before && self.assocs.contains_key(&item)
    }

    fn ask(&self) {
        self.asked.set(self.asked.get() + 1);
    }
}

impl Facts for World {
    fn trait_def(&self, _: Seal, t: Def) -> Fetch<TraitDef<'_>> {
        self.ask();
        let r = self.traits.get(&t).ok_or(Missing)?;
        Ok(TraitDef {
            flags: r.flags,
            generics: r.generics,
            lang: r.lang,
            supertraits: &r.supers,
            where_clauses: &r.wheres,
            assoc: &r.assoc,
            item_bounds: &r.item_bounds,
            portable: self.portable(),
            digest: r.digest,
        })
    }

    fn impls(&self, _: Seal, t: Def, shape: Shape) -> Fetch<ImplSet<'_>> {
        self.ask();
        static NONE: [Def; 0] = [];
        let b = match self.buckets.get(&(t, shape)) {
            Some(b) => b,
            None => match self.buckets.get(&(t, Shape::Blanket)) {
                Some(b) => b,
                None => {
                    if !self.traits.contains_key(&t) {
                        return Err(Missing);
                    }
                    return Ok(ImplSet { impls: &NONE, columns: Columns::EMPTY, digest: 0 });
                }
            },
        };
        Ok(ImplSet {
            impls: &b.impls,
            columns: Columns { bytes: &b.columns, stride: b.stride, len: b.impls.len() as u32 },
            digest: b.digest,
        })
    }

    fn impl_header(&self, _: Seal, i: Def) -> Fetch<ImplHeader<'_>> {
        self.ask();
        let r = self.impls.get(&i).ok_or(Missing)?;
        Ok(ImplHeader {
            generics: r.generics,
            polarity: r.polarity,
            flags: r.flags,
            trait_: r.trait_,
            args: r.args,
            where_clauses: &r.wheres,
            assoc: &r.assoc,
            portable: self.portable(),
            digest: r.digest,
        })
    }

    fn adt(&self, _: Seal, d: Def) -> Fetch<AdtDef<'_>> {
        self.ask();
        let r = self.adts.get(&d).ok_or(Missing)?;
        Ok(AdtDef {
            flags: r.flags,
            generics: r.generics,
            fields: &r.fields,
            variants: &r.variants,
            sized_tail: r.sized_tail,
            // The test language writes no `repr`: an enum has the default one.
            discr: if r.flags.has(AdtFlags::ENUM) {
                crate::facts::Discr::Int(crate::term::IntTy::Isize)
            } else {
                crate::facts::Discr::Unread
            },
            where_clauses: &r.wheres,
            portable: self.portable(),
            digest: r.digest,
        })
    }

    fn fn_sig(&self, _: Seal, d: Def) -> Fetch<FnSigDef<'_>> {
        self.ask();
        let r = self.fns.get(&d).ok_or(Missing)?;
        Ok(FnSigDef { generics: r.generics, inputs_output: r.io, flags: r.flags, portable: self.portable(), digest: r.digest })
    }

    fn assoc(&self, _: Seal, d: Def) -> Fetch<AssocDef> {
        self.ask();
        self.assocs.get(&d).copied().ok_or(Missing)
    }

    fn opaque(&self, _: Seal, d: Def) -> Fetch<OpaqueDef<'_>> {
        self.ask();
        let r = self.opaques.get(&d).ok_or(Missing)?;
        Ok(OpaqueDef { generics: r.generics, bounds: &r.bounds, hidden: r.hidden, portable: self.portable(), digest: r.digest })
    }

    fn lang(&self, _: Seal, l: Lang) -> Fetch<Option<Def>> {
        self.ask();
        Ok(self.langs.get(&l).copied())
    }

    fn is_local(&self, _: Seal, d: Def) -> Fetch<bool> {
        self.ask();
        Ok(self.local.contains(&d))
    }

    /// The test world writes no specializing impls: two impls that both apply overlap.
    fn specializes(&self, _: Seal, _: Def, _: Def) -> Fetch<bool> {
        self.ask();
        Ok(false)
    }

    /// By the condition's name (`PredA::Const`), whatever the arguments.
    fn const_holds(&self, _: Seal, cond: Def, _: &[Ty]) -> Fetch<crate::facts::ConstHolds> {
        self.ask();
        use crate::facts::ConstHolds;
        [("yes", ConstHolds::True), ("no", ConstHolds::False), ("err", ConstHolds::Error)]
            .into_iter()
            .find(|&(name, _)| def_of("const", name) == cond)
            .map(|(_, v)| v)
            .ok_or(Missing)
    }
}
