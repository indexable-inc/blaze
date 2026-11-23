//! THE VARIANCE OF AN ADT'S PARAMETERS, every one: lifetimes, types and consts (the Reference,
//! "Subtyping and Variance": "the variance of other struct, enum, and union types is decided
//! by looking at the variance of the types of their fields"; D190 point 2). The sans-IO core:
//! the lattice ([`Solving`]), a field type laid out over its region column ([`lay_out`]), and
//! the constraints one field puts on its ADT's parameters ([`constrain_field`]). The fixed point
//! over a strongly connected group of ADTs, its record and its key are adt_facts.rs's.
//!
//! The rules, the Reference's table row by row (rustc_hir_analysis variance/constraints.rs is
//! the reference implementation; it is read for the algebra, not ported):
//!
//! - `&'a T`: covariant in `'a` and `T`; `&'a mut T`: covariant in `'a`, invariant in `T`;
//! - `*const T` covariant, `*mut T` invariant; `[T]`, `[T; n]`, tuples covariant;
//! - `fn(T) -> U`: contravariant in `T`, covariant in `U`; a region its binder binds is no
//!   parameter's (`Region::Late`);
//! - `dyn Trait<T> + 'a`: covariant in `'a`, invariant in `T` and in every projection bound;
//! - `<T as Tr>::A`, an opaque type: invariant in every argument;
//! - another ADT `S<A..>`: argument `j` at that ADT's variance of its parameter `j`, a fixed
//!   point where the two name each other;
//! - `PhantomData<T>` covariant, `UnsafeCell<T>` invariant: by lang item, no field read;
//! - a const parameter is invariant (it holds no region, so the answer moves nothing).
//!
//! A parameter no field uses stays bivariant inside the fixed point (`Bi`, rustc's
//! `Bivariant`: an ADT naming it relates nothing through it); it is answered invariant to the
//! borrow check, which knows no bivariance (sound: invariance asks more).

use blaze_own::{AVec, Alloc};
use blaze_typeck as tc;
use tc::regions::{Arity, NotKnown, Region, region_arity};

/// The variance lattice of the fixed point: `Bi` its top (no constraint yet), `In` its bottom.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Solving {
    Bi,
    Co,
    Contra,
    In,
}

impl Solving {
    /// The greatest lower bound: what both uses allow.
    pub const fn meet(self, o: Solving) -> Solving {
        match (self, o) {
            (Solving::Bi, x) | (x, Solving::Bi) => x,
            (Solving::In, _) | (_, Solving::In) => Solving::In,
            (Solving::Co, Solving::Co) => Solving::Co,
            (Solving::Contra, Solving::Contra) => Solving::Contra,
            (Solving::Co, Solving::Contra) | (Solving::Contra, Solving::Co) => Solving::In,
        }
    }

    /// A position of declared variance `d` met in a context of variance `self` (the
    /// transform: covariant keeps, contravariant flips, invariant and bivariant absorb).
    pub const fn then(self, d: Solving) -> Solving {
        match (self, d) {
            (Solving::Co, x) => x,
            (Solving::Contra, Solving::Co) => Solving::Contra,
            (Solving::Contra, Solving::Contra) => Solving::Co,
            (Solving::Contra, Solving::In) => Solving::In,
            (Solving::Contra, Solving::Bi) => Solving::Bi,
            (Solving::In, _) => Solving::In,
            (Solving::Bi, _) => Solving::Bi,
        }
    }

    /// The borrow check's answer: bivariant is answered invariant (the lowering has no `Bi`;
    /// invariance relates both ways, which bivariance allows).
    pub const fn answer(self) -> blaze_flow_lower::Variance {
        match self {
            Solving::Co => blaze_flow_lower::Variance::Co,
            Solving::Contra => blaze_flow_lower::Variance::Contra,
            Solving::Bi | Solving::In => blaze_flow_lower::Variance::In,
        }
    }

    /// A tag byte for a digest.
    pub const fn tag(self) -> u8 {
        match self {
            Solving::Bi => 0,
            Solving::Co => 1,
            Solving::Contra => 2,
            Solving::In => 3,
        }
    }
}

/// No parent: the root of a laid type.
pub const NO_PARENT: u32 = u32::MAX;

/// The most nodes one field type is laid out over (rule 18).
pub const MOST_LAID_NODES: u32 = 1 << 16;

/// One node of a type laid out over its region column, in the column's pre-order: its own
/// slots are `col[start..start + own]`, its whole subtree's `col[start..end]`, its subtree's
/// nodes `laid[its index..after]`. `parent` and `nth` place it under its parent (indices
/// relative to the root's; the root's parent `NO_PARENT`), `kids` counts its own children.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaidNode {
    pub ty: tc::PTy,
    pub node: tc::PNode,
    pub start: u32,
    pub own: u32,
    pub end: u32,
    pub parent: u32,
    pub nth: u32,
    pub kids: u32,
    pub after: u32,
}

/// The index of child `nth` of node `at` of `laid`, if it has one.
pub fn child_of(laid: &[LaidNode], at: usize, nth: u32) -> Option<usize> {
    let n = laid.get(at)?;
    let mut c = at + 1;
    while c < n.after as usize {
        let k = laid.get(c)?;
        if k.nth == nth {
            return Some(c);
        }
        c = (k.after as usize).max(c + 1);
    }
    None
}

/// Why a type could not be laid out over its column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaidError {
    /// The portable table could not be read.
    Table(tc::CheckError),
    /// A node's own region count is not known.
    Count(NotKnown),
    /// A body's type (a hole, a closure, a future, a coroutine) in a declared type.
    BodyType { node: tc::PTy },
    /// The column holds `got` slots where the walk counted `want` (the one counter and the
    /// column's producer disagree: rule 76).
    Column { ty: tc::PTy, want: u32, got: u32 },
    /// More than [`MOST_LAID_NODES`] nodes, or more than `u32::MAX` slots.
    Bound { ty: tc::PTy },
}

enum Step {
    Enter { ty: tc::PTy, parent: u32, nth: u32 },
    Exit(usize),
}

/// Whether a region is bound by a binder inside the type (a fn pointer's `for<'x>`).
pub const fn is_late(r: Region) -> bool {
    matches!(r, Region::Late { .. })
}

/// The type children of `node`, in the order the one region counter lays their slots
/// (blaze-typeck regions.rs `kids`): a list, then one more (a reference's pointee, an array's
/// element, a projection bound's term).
pub fn children<'t>(tys: tc::PTys<'t>, t: tc::PTy, node: tc::PNode) -> Result<(&'t [tc::PTy], Option<tc::PTy>), LaidError> {
    use tc::PNode as N;
    let list = |l| tys.list(l).map_err(LaidError::Table);
    Ok(match node {
        N::Tuple(l) | N::Record(_, l) | N::Union(l) | N::Adt(_, l) | N::FnDef(_, l) | N::FnPtr(l, _) | N::Opaque(_, l) | N::Alias { args: l, .. } | N::Dynamic(l) | N::ExTrait(_, l) => (list(l)?, None),
        N::Ref(_, c) | N::Ptr(_, c) | N::Slice(c) | N::Array(c, _) | N::Pat(c, _) => (&[], Some(c)),
        N::ExProj { args, term, .. } => (list(args)?, Some(term)),
        N::Infer | N::Closure { .. } | N::Future(..) | N::Coroutine { .. } => return Err(LaidError::BodyType { node: t }),
        N::Bool
        | N::Char
        | N::Int(_)
        | N::Uint(_)
        | N::Float(_)
        | N::Str
        | N::Never
        | N::Param(_)
        | N::Error
        | N::ConstValue { .. }
        | N::ConstParam(_)
        | N::ExAuto(_)
        | N::Foreign(_) => (&[], None),
    })
}

/// Lays `ty` out over a column of `col_len` slots, appending its nodes to `out` in pre-order
/// (indices and parents relative to the first appended). The slot cursor must end at
/// `col_len`, or the column is not this type's.
pub fn lay_out<F: Arity + ?Sized>(tys: tc::PTys<'_>, ty: tc::PTy, arity: &F, col_len: u32, out: &mut AVec<'_, LaidNode>, alloc: Alloc<'_>) -> Result<(), LaidError> {
    let base = out.len();
    let mut stack: AVec<'_, Step> = AVec::new_in(alloc);
    stack.push(Step::Enter { ty, parent: NO_PARENT, nth: 0 });
    let mut cursor = 0u32;
    let mut budget = MOST_LAID_NODES;
    while let Some(step) = stack.pop() {
        match step {
            Step::Exit(at) => {
                let after = u32::try_from(out.len() - base).map_err(|_| LaidError::Bound { ty })?;
                if let Some(n) = out.get_mut(at) {
                    n.end = cursor;
                    n.after = after;
                }
            }
            Step::Enter { ty: t, parent, nth } => {
                if budget == 0 {
                    return Err(LaidError::Bound { ty });
                }
                budget -= 1;
                let node = tys.node(t).map_err(LaidError::Table)?;
                let own = region_arity(t, node, arity).map_err(LaidError::Count)?;
                let (list, last) = children(tys, t, node)?;
                let kids = u32::try_from(list.len() + usize::from(last.is_some())).map_err(|_| LaidError::Bound { ty })?;
                let at = out.len();
                let me = u32::try_from(at - base).map_err(|_| LaidError::Bound { ty })?;
                out.push(LaidNode { ty: t, node, start: cursor, own, end: cursor, parent, nth, kids, after: me + 1 });
                cursor = cursor.checked_add(own).ok_or(LaidError::Bound { ty })?;
                stack.push(Step::Exit(at));
                // Reversed onto the stack, so the first child is entered first.
                if let Some(c) = last {
                    stack.push(Step::Enter { ty: c, parent: me, nth: kids - 1 });
                }
                for (j, &c) in list.iter().enumerate().rev() {
                    stack.push(Step::Enter { ty: c, parent: me, nth: j as u32 });
                }
            }
        }
    }
    if cursor != col_len {
        return Err(LaidError::Column { ty, want: cursor, got: col_len });
    }
    Ok(())
}

/// Why a field's constraints could not be taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VarianceRefusal {
    /// A region or type parameter `index` of an ADT with `count` of that kind.
    ParamPast { index: u32, count: u32 },
    /// A field names the ADT `adt`, whose variance of parameter `index` is not known (its
    /// facts refused, or not asked): never guessed (rule 3).
    Unsolved { adt: u128, index: u32 },
    /// The fixed point did not settle in `rounds` rounds (it must within twice the parameter
    /// count plus one: a bug, named).
    Rounds { rounds: u32 },
    /// A lang ADT (`PhantomData`, `UnsafeCell`) whose head has not the one type parameter
    /// and no lifetime its lang item has.
    LangShape { lifetimes: u32, generics: u32 },
}

/// The variance an ADT declares for its parameter `index` (lifetimes first, then its other
/// generics in order), as the fixed point stands.
pub trait VarianceOf {
    fn variance(&self, adt: u128, index: u32) -> Result<Solving, VarianceRefusal>;
}

/// Meets into `out` (the ADT's parameters: `lifetimes` lifetimes, then `generics` others)
/// the constraints of one field laid out as `laid` over `col`, the field in covariant
/// position. `ctx` is scratch.
pub fn constrain_field(
    laid: &[LaidNode],
    col: &[Region],
    lifetimes: u32,
    generics: u32,
    of: &dyn VarianceOf,
    ctx: &mut AVec<'_, Solving>,
    out: &mut [Solving],
) -> Result<(), VarianceRefusal> {
    use tc::PNode as N;
    ctx.clear();
    for (i, n) in laid.iter().enumerate() {
        let v = if n.parent == NO_PARENT {
            Solving::Co
        } else {
            let Some(&p) = laid.get(n.parent as usize) else { return Err(VarianceRefusal::ParamPast { index: n.parent, count: i as u32 }) };
            let pv = ctx.get(n.parent as usize).copied().unwrap_or(Solving::In);
            child_variance(p, pv, n.nth, of)?
        };
        ctx.push(v);
        if v == Solving::Bi {
            continue;
        }
        // Its own slots.
        for s in 0..n.own {
            let Some(&r) = col.get((n.start + s) as usize) else { return Err(VarianceRefusal::ParamPast { index: n.start + s, count: col.len() as u32 }) };
            let Region::Param(x) = r else { continue };
            let at = match n.node {
                N::Ref(..) | N::Dynamic(_) => v,
                N::Adt(k, _) => declared(v, k.0, s, of)?,
                _ => v.then(Solving::In),
            };
            let Some(slot) = out.get_mut(x as usize).filter(|_| x < lifetimes) else {
                return Err(VarianceRefusal::ParamPast { index: x, count: lifetimes });
            };
            *slot = slot.meet(at);
        }
        let generic = |p: u32| -> Result<usize, VarianceRefusal> {
            if p >= generics {
                return Err(VarianceRefusal::ParamPast { index: p, count: generics });
            }
            Ok((lifetimes + p) as usize)
        };
        match n.node {
            N::Param(p) => {
                let at = generic(p)?;
                if let Some(slot) = out.get_mut(at) {
                    *slot = slot.meet(v);
                }
            }
            N::ConstParam(p) => {
                let at = generic(p)?;
                if let Some(slot) = out.get_mut(at) {
                    *slot = slot.meet(Solving::In);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// The variance ADT `adt` declares for `index`, met in context `v`: asked only where it can
/// matter (an invariant context is invariant whatever the ADT declares).
fn declared(v: Solving, adt: u128, index: u32, of: &dyn VarianceOf) -> Result<Solving, VarianceRefusal> {
    match v {
        Solving::Bi | Solving::In => Ok(v),
        Solving::Co | Solving::Contra => Ok(v.then(of.variance(adt, index)?)),
    }
}

/// The context of child `nth` of `p`, whose context is `pv`.
fn child_variance(p: LaidNode, pv: Solving, nth: u32, of: &dyn VarianceOf) -> Result<Solving, VarianceRefusal> {
    use tc::PNode as N;
    Ok(match p.node {
        N::Ref(m, _) | N::Ptr(m, _) => pv.then(match m {
            tc::Mutbl::Not => Solving::Co,
            tc::Mutbl::Mut => Solving::In,
        }),
        N::Slice(_) | N::Array(..) | N::Pat(..) | N::Tuple(_) | N::Record(..) | N::Union(_) => pv,
        // Argument `nth` is the ADT's parameter `lifetimes + nth`: its lifetimes are its own
        // slots, laid before its children.
        N::Adt(k, _) => declared(pv, k.0, p.own + nth, of)?,
        N::FnPtr(..) => {
            if nth + 1 == p.kids {
                pv
            } else {
                pv.then(Solving::Contra)
            }
        }
        _ => pv.then(Solving::In),
    })
}

/// The fixed variances of a lang ADT: `PhantomData<T>` covariant in `T`, `UnsafeCell<T>`
/// invariant (the two rustc fixes by lang item, `variance::terms::lang_items`).
pub fn lang_variances(lang: AdtLang, lifetimes: u32, generics: u32) -> Result<Option<Solving>, VarianceRefusal> {
    let fixed = match lang {
        AdtLang::Plain => return Ok(None),
        AdtLang::PhantomData => Solving::Co,
        AdtLang::UnsafeCell => Solving::In,
    };
    if lifetimes != 0 || generics != 1 {
        return Err(VarianceRefusal::LangShape { lifetimes, generics });
    }
    Ok(Some(fixed))
}

/// Which lang item an ADT is, as far as variance and outlives are concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AdtLang {
    Plain,
    PhantomData,
    UnsafeCell,
}
