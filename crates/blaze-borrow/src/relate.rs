//! Two types related by variance, and clauses turned into outlives (end-state.md D156; the
//! audit's C5).
//!
//! THE WALK. `relate(a, b, v)` says "a value of `a` flows where `b` is wanted" (`a <: b` when
//! `v` is covariant). The two types are walked IN STEP: the same head (one variant, one item,
//! one mutability, one arity), each own slot paired with the other's, each child walked under
//! its position's variance composed with the parent's. A region pair under covariance emits
//! `a_r: b_r` (the subtype's region outlives the supertype's), under contravariance the
//! reverse, under invariance both. Two heads that differ are [`Refused::Shape`]: the type
//! checker accepted the program, so a shape mismatch is a missing normalization or a bug, never
//! guessed around.
//!
//! WHAT A SIDE IS. A caller's value has its slots in the caller's origin column; a signature's
//! type has its declared column, read through an environment (inst.rs): a callee's `Param(i)`
//! is the call's region for it, a callee's type parameter goes on in the caller's type, an
//! impl's parameters are bound by walking its header against what it was selected for (each
//! first meeting binds, a second meeting relates invariantly: `impl<'a> T<'a> for &'a U` ties
//! the two).
//!
//! CLAUSES. `T: 'r` holds when every component of `T` outlives `'r` (RFC 1214): each region
//! slot, except a region a binder inside `T` binds; for `dyn Tr + 'o` only `'o`; for the
//! caller's own type parameter `P`, its origin `ρ_P` (whose known edges are the caller's bounds
//! on `P`: param_outlives.rs's rule), so the requirement lands against the caller's bounds;
//! for an alias, its selected impl's value, or else its arguments. `'a: 'b` is one subset.
//! `S: Trait<'x..>` ties `'x` to the impl that proves it (selected at this call) or to the
//! caller's own bound when `S` is the caller's parameter.
#![blaze::sans_io]

use core::alloc::Allocator;
use core::cell::Cell;

use blaze_flow::Origin;
use blaze_typeck::region_facts::Outlives;
use blaze_typeck::regions::Region;
use blaze_typeck::{ItemKey, PNode, PTy};

use crate::inst::{Arg, Ask, AskArgs, Clause, EnvId, Envs, ImplValue, Inst, Keyed, RegAt, Selected, Selection, Side, Slots, TypeAt, Typed};
use crate::types::{Kids, Pos, RegionTypes, SlotCount, Unknown};
use crate::{Bad, Head, NotSelected, Refused, Reg, ShapeWhy, Variance};

/// What a fresh origin is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// An inference origin of this body (every region a call instantiates).
    Existential,
    /// A universal one (a placeholder; not made by the call rule today).
    Placeholder,
}

/// THE EMIT SEAM: all the call rule says to the flow. `subset(a, b)` is `'a: 'b` (`a`'s loans
/// flow into `b`: blaze-flow's `subset_base(from, into)`).
pub trait Constraints {
    fn subset(&mut self, a: Origin, b: Origin);
    /// `'a: 'static`.
    fn outlives_static(&mut self, a: Origin);
    fn fresh(&mut self, kind: Kind) -> Origin;
}

/// A closure of the caller body's signature, laid over its `PNode::FnPtr([in.., out])` in the
/// caller's type table (`Cx::types`): `ncap` capture slots precede `nsig` owner signature slots
/// in the closure value's own slots, and `col` maps each slot of `sig_ty` to
/// `Region::Param(j)` (for `j < nsig`, the `(ncap + j)`-th slot of the closure value),
/// `Region::Static`, or `Region::Late { binder: 0, var }` (bound per call).
#[derive(Clone, Copy, Debug)]
pub struct ClosureSig<'c> {
    pub sig_ty: PTy,
    pub ncap: u32,
    pub nsig: u32,
    pub col: &'c [Region],
}

/// The caller: the body whose flow is being built.
pub trait Owner {
    fn item(&self) -> ItemKey;
    /// The universal origin of the caller's own region `r` (its lifetime parameter, in its
    /// numbering); `None` if it has none.
    fn universal(&self, r: Region) -> Option<Origin>;
    /// `ρ_P`: the origin standing for the caller's type parameter `p` (its `PNode::Param`
    /// index), whose known edges are the caller's `P: 'x` bounds.
    fn param(&self, p: u32) -> Option<Origin>;
    /// The caller's bound `P: trait_<..>` on its parameter `p`: the `ExTrait` node and its
    /// column in the caller's numbering.
    fn bound(&self, p: u32, trait_: ItemKey) -> Option<Typed<'_>>;
    /// The caller's projection bound `P: trait_<.., name = U>` on its parameter `p`: the `ExProj`
    /// node and its column in the caller's numbering.
    fn proj_bound(&self, p: u32, trait_: ItemKey, name: blaze_typeck::Name) -> Option<Typed<'_>> {
        let _ = (p, trait_, name);
        None
    }
    /// All declared `ExTrait` / `ExProj` clauses of the caller (`regions.clauses(owner)`),
    /// in the caller's numbering (`owner_env`), for compound subjects and supertrait elaboration.
    fn clauses(&self) -> &[blaze_typeck::region_facts::ClauseView<'_>] {
        &[]
    }
    /// Signature of closure `ordinal` of this body (`owner == self.item()`), when known.
    fn closure_sig(&self, ordinal: u32) -> Option<ClosureSig<'_>> {
        let _ = ordinal;
        None
    }
    /// The caller's type parameters that `body`, a body type of the caller in its own table
    /// (a closure, an async block's future), holds by its captures at any depth; `None`: not
    /// given, and a components walk reaching it is refused ([`Bad::BodyParamsUnread`]). Its
    /// own slots (the captures' regions) are the walk's as any node's; a captured `P` has no
    /// slot, so `body: 'r` asks `ρ_P: 'r` through this (rustc: a closure's outlives
    /// components are its upvar types).
    fn body_params(&self, body: PTy) -> Option<&[u32]>;
}

/// What the call rule reads and where it writes.
///
/// TWO TABLES. A caller's value ([`Slots::Caller`]) is a type of the body's table (`types`);
/// a declared type ([`Slots::Sig`]: a signature's, a clause's, an impl header's or value's, the
/// caller's own bound) is a type of the heads' table (`sig_types`). Each side is read in its
/// own table and only there: a `PTy` of one is never a node of the other. Where both are one
/// table, both fields are the same answer.
pub struct Cx<'c, 'x> {
    pub types: &'c dyn RegionTypes,
    pub sig_types: &'c dyn RegionTypes,
    pub selected: &'c dyn Selected,
    pub owner: &'c dyn Owner,
    /// The caller's origin column: `Reg::Slot(s)` is `origins[s]`.
    pub origins: &'x [Origin],
    pub out: &'x mut dyn Constraints,
}

/// The most steps one call's walks take (rule 18).
pub const MOST_WALK_STEPS: u32 = 1 << 16;
/// How deep one walk goes, normalizations included.
pub const MOST_WALK_DEPTH: u32 = 256;

/// The most bound-variable pairs one relation keeps (rule 18): past it, refused by name.
pub const MOST_BOUND_PAIRS: usize = 32;
/// How deep impls' where-clauses nest (an impl's clause tied through another impl ..; rule 18).
pub const MOST_IMPL_NESTING: u32 = 32;

/// A region ready to emit.
#[derive(Clone, Copy, PartialEq, Eq)]
enum At {
    O(Origin),
    Static,
}

/// A bound region as one side of a relation reads it: the binder's index in its laid column's
/// pre-order (blaze-typeck regions.rs `Region::Late`), keyed by the environment the column is
/// read in, so two columns' binders numbered alike stay apart.
#[derive(Clone, Copy, PartialEq, Eq)]
struct BoundAt {
    env: Option<EnvId>,
    binder: u32,
    var: u32,
}

const WALK_STACK_CAP: usize = 512;
const COMP_STACK_CAP: usize = 512;

/// Higher-ranked binder Skolemization context for the current walk.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BinderCtx {
    None,
    /// Inside `PNode::FnPtr`: `Co`/`Contra` Skolemizes, `In` refuses (`HigherRanked`).
    FnPtr(Variance),
    /// Inside `PNode::Dynamic`: `Co`/`Contra` Skolemizes; at `In`, when one side is
    /// `Slots::Caller` (which carries caller-erased `Origin` slots for `dyn`'s `for<..>`),
    /// Skolemizes the `Slots::Sig` binder to a rigid `Kind::Placeholder`.
    Dynamic(Variance),
    /// Proving a goal (`b`) from a proven `for<..>` clause (`a`): `a`'s bound vars are
    /// `Kind::Existential`, `b`'s are `Kind::Placeholder`.
    Clause,
}

#[derive(Clone, Copy)]
struct WalkFrame<'c> {
    a_ty: PTy,
    b_ty: PTy,
    ra: Slots<'c>,
    rb: Slots<'c>,
    v: Variance,
    binder_v: BinderCtx,
    next_k: u32,
    depth: u32,
}

#[derive(Clone, Copy)]
struct CompFrame<'c> {
    ty: PTy,
    rest: Slots<'c>,
    next_k: u32,
    depth: u32,
}

struct Walk<'w, 'c, A: Allocator + Copy> {
    types: &'c dyn RegionTypes,
    sig_types: &'c dyn RegionTypes,
    selected: &'c dyn Selected,
    owner: &'c dyn Owner,
    origins: &'w [Origin],
    out: &'w mut dyn Constraints,
    envs: &'w mut Envs<'c, A>,
    item: ItemKey,
    budget: u32,
    /// ALPHA RENAMING: the bound variables met in step in this relation, paired one to one
    /// ([`Walk::bound_pair`]). Cleared where a relation starts (a walk at depth 0).
    bound: [Option<(BoundAt, BoundAt)>; MOST_BOUND_PAIRS],
    bound_len: usize,
    /// HIGHER-RANKED SKOLEMIZATION: bound variables instantiated to fresh `Existential`
    /// (subtype) or `Placeholder` (supertype) origins within the current relation.
    hr: [Option<(BoundAt, Origin)>; MOST_BOUND_PAIRS],
    hr_len: usize,
    /// Context of the innermost enclosing `FnPtr`, `Dynamic`, or `for<..>` clause binder.
    binder_v: BinderCtx,
    /// How many impls' where-clauses are being related around this point ([`Walk::impl_where`]).
    nested: u32,
    walk_stack: [Option<WalkFrame<'c>>; WALK_STACK_CAP],
    walk_sp: usize,
    comp_stack: [Option<CompFrame<'c>>; COMP_STACK_CAP],
    comp_sp: usize,
}

/// The ask's arguments `[Self, trait args..]`, where a side holds them.
#[derive(Clone, Copy)]
enum AskAt<'c> {
    /// An alias, or a bound laid subject-first (`ExTrait(Tr, [T, A..])`): its children.
    Kids(Side<'c>),
    /// A bound with its subject apart (`ExTrait(Tr, [A..])`): the subject, then the children.
    Subject { subject: Side<'c>, of: Side<'c> },
    /// Subject-first node (`ExTrait`, `Alias`, or `ExProj` of `len` args) whose child 0 has
    /// already been normalized to `subject`, with remaining arguments `1..len` from `of`.
    SubjectKids { subject: Side<'c>, of: Side<'c>, len: u32 },
}

/// [`AskArgs`] over a walk: each argument followed through its environment and compared with a
/// row's type erased (`Walk::erased`), within one step budget for the whole ask.
struct Keyer<'k, 'w, 'c, A: Allocator + Copy> {
    walk: &'k Walk<'w, 'c, A>,
    at: AskAt<'c>,
    budget: Cell<u32>,
}

impl<'c, A: Allocator + Copy> Keyer<'_, '_, 'c, A> {
    /// The `k`-th argument.
    fn arg(&self, k: u32) -> Option<Side<'c>> {
        match self.at {
            AskAt::Kids(s) => self.walk.erased_kid(s, k),
            AskAt::Subject { subject, .. } if k == 0 => Some(subject),
            AskAt::Subject { of, .. } => self.walk.erased_kid(of, k - 1),
            AskAt::SubjectKids { subject, .. } if k == 0 => Some(subject),
            AskAt::SubjectKids { of, len, .. } if k < len => self.walk.erased_kid(of, k),
            AskAt::SubjectKids { .. } => None,
        }
    }
}

impl<A: Allocator + Copy> AskArgs for Keyer<'_, '_, '_, A> {
    fn count(&self) -> Option<u32> {
        match self.at {
            AskAt::Kids(s) => self.walk.kids_of(s).ok().map(|k| k.len()),
            AskAt::Subject { of, .. } => self.walk.kids_of(of).ok().and_then(|k| k.len().checked_add(1)),
            AskAt::SubjectKids { len, .. } => Some(len),
        }
    }

    fn compare(&self, k: u32, ty: PTy) -> Keyed {
        match self.arg(k) {
            Some(side) => self.walk.erased(side, ty, 0, &self.budget),
            None => Keyed::Unknown,
        }
    }

    fn self_shape(&self) -> Option<crate::inst::SelfShape> {
        let side = self.arg(0)?;
        let Ok(Resolved::Side(s)) = self.walk.resolve(side) else { return None };
        match self.walk.node(s) {
            Ok(PNode::Adt(k, _)) => Some(crate::inst::SelfShape::Adt(k)),
            Ok(PNode::Ref(m, _)) => Some(crate::inst::SelfShape::Ref(m)),
            Ok(PNode::Tuple(_)) => self.walk.kids_of(s).ok().map(|k| crate::inst::SelfShape::Tuple(k.len())),
            _ => None,
        }
    }
}

/// A side after its type parameters are followed.
#[derive(Clone, Copy)]
enum Resolved<'c> {
    Side(Side<'c>),
    /// An impl's type parameter not bound yet.
    Unbound { env: crate::inst::EnvId, param: u32 },
}

impl<'w, 'c, A: Allocator + Copy> Walk<'w, 'c, A> {
    fn malformed(&self, node: PTy, what: Bad) -> Refused {
        Refused::Malformed { item: self.item, node, what }
    }

    fn count_err(&self, unknown: Unknown) -> Refused {
        Refused::Count { item: self.item, unknown }
    }

    fn step(&mut self, node: PTy, depth: u32) -> Result<(), Refused> {
        if self.budget == 0 || depth > MOST_WALK_DEPTH {
            return Err(self.malformed(node, Bad::StepBound));
        }
        self.budget -= 1;
        Ok(())
    }

    /// The table a side's type is a node of ([`Cx`], "TWO TABLES").
    fn tab(&self, s: Slots<'c>) -> &'c dyn RegionTypes {
        match s {
            Slots::Caller(_) | Slots::CallerEnv { .. } => self.types,
            Slots::Sig { .. } => self.sig_types,
        }
    }

    fn node(&self, s: Side<'c>) -> Result<PNode, Refused> {
        self.tab(s.slots).node(s.ty).map_err(|u| self.count_err(u))
    }

    fn own(&self, s: Side<'c>) -> Result<SlotCount, Refused> {
        self.tab(s.slots).own(s.ty).map_err(|u| self.count_err(u))
    }

    fn kids_of(&self, s: Side<'c>) -> Result<Kids<'c>, Refused> {
        self.tab(s.slots).kids(s.ty).map_err(|u| self.count_err(u))
    }

    /// Follow a signature side's type parameter to what the call or a header walk bound it to.
    fn resolve(&self, mut s: Side<'c>) -> Result<Resolved<'c>, Refused> {
        for _ in 0..MOST_WALK_DEPTH {
            let Slots::Sig { env, .. } = s.slots else { return Ok(Resolved::Side(s)) };
            let PNode::Param(t) = self.node(s)? else { return Ok(Resolved::Side(s)) };
            match self.envs.type_of(env, t).map_err(|w| self.malformed(s.ty, w))? {
                TypeAt::Bound(next) => s = next,
                TypeAt::Rigid => return Ok(Resolved::Side(s)),
                TypeAt::Unbound { env, param } => return Ok(Resolved::Unbound { env, param }),
            }
        }
        Err(self.malformed(s.ty, Bad::StepBound))
    }

    /// The `k`-th slot of a side, through its environment.
    fn slot(&self, s: Side<'c>, k: u32) -> Result<RegAt, Refused> {
        match s.slots {
            Slots::Caller(r) => {
                let slot = r.nth(k).ok_or(self.malformed(s.ty, Bad::StepBound))?;
                Ok(RegAt::Known(Reg::Slot(slot)))
            }
            Slots::Sig { col, env } | Slots::CallerEnv { col, env } => {
                let r = *col.get(k as usize).ok_or(self.malformed(s.ty, Bad::StepBound))?;
                self.envs.reg(env, r).map_err(|w| self.malformed(s.ty, w))
            }
        }
    }

    /// A region as emitted.
    fn at(&self, node: PTy, r: Reg) -> Result<At, Refused> {
        match r {
            Reg::Static | Reg::Owner(Region::Static) => Ok(At::Static),
            Reg::Origin(o) => Ok(At::O(o)),
            Reg::Slot(s) => self.origins.get(s.index()).map(|&o| At::O(o)).ok_or(self.malformed(node, Bad::SlotPast { slot: s })),
            Reg::Owner(region) => self.owner.universal(region).map(At::O).ok_or(self.malformed(node, Bad::NoUniversal { region })),
            Reg::Bound { binder, .. } => Err(Refused::HigherRanked { item: self.item, node, binder }),
        }
    }

    /// `long: short`.
    fn outlives(&mut self, node: PTy, long: Reg, short: Reg) -> Result<(), Refused> {
        if long == short {
            return Ok(());
        }
        match (self.at(node, long)?, self.at(node, short)?) {
            (At::Static, _) => {}
            (At::O(a), At::Static) => self.out.outlives_static(a),
            (At::O(a), At::O(b)) => {
                if a != b {
                    self.out.subset(a, b);
                }
            }
        }
        Ok(())
    }

    /// Look up or instantiate a higher-ranked bound variable as an existential (subtype) or
    /// placeholder (supertype) origin within the current relation.
    fn hr_origin(&mut self, node: PTy, key: BoundAt, kind: Kind) -> Result<Origin, Refused> {
        if let Some(used) = self.hr.get(..self.hr_len) {
            for &(k, o) in used.iter().flatten() {
                if k == key {
                    return Ok(o);
                }
            }
        }
        let Some(slot) = self.hr.get_mut(self.hr_len) else {
            return Err(self.malformed(node, Bad::StepBound));
        };
        let o = self.out.fresh(kind);
        *slot = Some((key, o));
        self.hr_len += 1;
        Ok(o)
    }

    fn hr_has(&self, key: BoundAt) -> bool {
        self.hr.get(..self.hr_len).is_some_and(|used| used.iter().flatten().any(|&(k, _)| k == key))
    }

    /// One slot pair under `v`: `a` the subtype's region, `b` the supertype's, read in the
    /// environments `ea` and `eb` (`None`: the caller's slots). A first meeting of an impl's
    /// parameter binds it; two bound regions are paired by [`Walk::bound_pair`]; a bound region
    /// meeting a free/existential region under a `Co`/`Contra` binder is Skolemized (subtype
    /// binder -> `Existential`, supertype binder -> `Placeholder`).
    fn pair(&mut self, node: PTy, a: RegAt, b: RegAt, v: Variance, ea: Option<EnvId>, eb: Option<EnvId>) -> Result<(), Refused> {
        let (mut a, mut b) = match (a, b) {
            (RegAt::Known(a), RegAt::Known(b)) => (a, b),
            (RegAt::Unbound { env, param }, RegAt::Known(b)) => {
                return self.envs.bind_reg(env, param, b).map_err(|w| self.malformed(node, w));
            }
            (RegAt::Known(a), RegAt::Unbound { env, param }) => {
                return self.envs.bind_reg(env, param, a).map_err(|w| self.malformed(node, w));
            }
            (RegAt::Unbound { param, .. }, RegAt::Unbound { .. }) => return Err(self.malformed(node, Bad::Unconstrained { index: param })),
        };
        if let (Reg::Bound { binder: x, var: y }, Reg::Bound { binder: p, var: q }) = (a, b) {
            let ka = BoundAt { env: ea, binder: x, var: y };
            let kb = BoundAt { env: eb, binder: p, var: q };
            if !self.hr_has(ka) && !self.hr_has(kb) {
                return self.bound_pair(node, ka, kb);
            }
        }
        if let Reg::Bound { binder, var } = a {
            let kind = match self.binder_v {
                BinderCtx::FnPtr(Variance::Co) | BinderCtx::Dynamic(Variance::Co) | BinderCtx::Clause => Kind::Existential,
                BinderCtx::FnPtr(Variance::Contra) | BinderCtx::Dynamic(Variance::Contra) => Kind::Placeholder,
                BinderCtx::Dynamic(Variance::In) if eb.is_none() => Kind::Placeholder,
                BinderCtx::None | BinderCtx::FnPtr(Variance::In) | BinderCtx::Dynamic(Variance::In) => {
                    return Err(Refused::HigherRanked { item: self.item, node, binder });
                }
            };
            a = Reg::Origin(self.hr_origin(node, BoundAt { env: ea, binder, var }, kind)?);
        }
        if let Reg::Bound { binder, var } = b {
            let kind = match self.binder_v {
                BinderCtx::FnPtr(Variance::Co) | BinderCtx::Dynamic(Variance::Co) | BinderCtx::Clause => Kind::Placeholder,
                BinderCtx::FnPtr(Variance::Contra) | BinderCtx::Dynamic(Variance::Contra) => Kind::Existential,
                BinderCtx::Dynamic(Variance::In) if ea.is_none() => Kind::Placeholder,
                BinderCtx::None | BinderCtx::FnPtr(Variance::In) | BinderCtx::Dynamic(Variance::In) => {
                    return Err(Refused::HigherRanked { item: self.item, node, binder });
                }
            };
            b = Reg::Origin(self.hr_origin(node, BoundAt { env: eb, binder, var }, kind)?);
        }
        match v {
            Variance::Co => self.outlives(node, a, b),
            Variance::Contra => self.outlives(node, b, a),
            Variance::In => {
                self.outlives(node, a, b)?;
                self.outlives(node, b, a)
            }
        }
    }

    /// Two bound regions met in step. ALPHA RENAMING (the Reference, "Subtyping and Variance":
    /// `for<'a> fn(&'a u8)` and `for<'b> fn(&'b u8)` are one type, whatever each binder's index
    /// in its own column): each side's variable is paired with the other's at its first meeting,
    /// and every later meeting must name the same partner on BOTH sides. A bijection relates
    /// nothing outside the binders (a bound region never escapes them), so it emits nothing. A
    /// variable met with a second partner is not a renaming (`for<'a> fn(&'a u8, &'a u8)`
    /// against `for<'b, 'c> fn(&'b u8, &'c u8)`): deciding it needs placeholders, so it is
    /// refused by name ([`Refused::HigherRanked`]), never related as if renamed.
    fn bound_pair(&mut self, node: PTy, a: BoundAt, b: BoundAt) -> Result<(), Refused> {
        if let Some(used) = self.bound.get(..self.bound_len) {
            for &(x, y) in used.iter().flatten() {
                match (x == a, y == b) {
                    (true, true) => return Ok(()),
                    (false, false) => {}
                    _ => return Err(Refused::HigherRanked { item: self.item, node, binder: a.binder }),
                }
            }
        }
        let Some(slot) = self.bound.get_mut(self.bound_len) else {
            return Err(self.malformed(node, Bad::StepBound));
        };
        *slot = Some((a, b));
        self.bound_len += 1;
        Ok(())
    }

    /// The environment a side's slots are read in (`None`: the caller's own slots).
    fn env_of(s: Side<'c>) -> Option<EnvId> {
        match s.slots {
            Slots::Caller(_) => None,
            Slots::Sig { env, .. } | Slots::CallerEnv { env, .. } => Some(env),
        }
    }

    /// Structural erased equality between two resolved sides (including `Alias`, `Opaque`,
    /// `Param` heads): used to match caller/trait/opaque where-clauses against an asked subject
    /// and trait arguments.
    fn erased_sides(&self, a: Side<'c>, b: Side<'c>) -> bool {
        self.erased_sides_with(a, b, false)
    }

    fn erased_sides_with(&self, a: Side<'c>, b: Side<'c>, allow_alias: bool) -> bool {
        const MAX_FRAMES: usize = 257;
        #[derive(Clone, Copy)]
        struct Frame<'c> {
            a: Side<'c>,
            b: Side<'c>,
            next_k: u32,
            depth: u32,
        }
        let mut stack: [Option<Frame<'c>>; MAX_FRAMES] = [None; MAX_FRAMES];
        let mut sp = 0usize;
        let mut budget = MOST_WALK_STEPS;
        let mut enter = Some((a, b, 0u32));
        loop {
            if let Some((a, b, d)) = enter.take() {
                if d > MOST_WALK_DEPTH || budget == 0 {
                    return false;
                }
                budget -= 1;
                let (Ok(Resolved::Side(a)), Ok(Resolved::Side(b))) = (self.resolve(a), self.resolve(b)) else {
                    return false;
                };
                let (Ok(na), Ok(nb)) = (self.node(a), self.node(b)) else {
                    return false;
                };
                if is_const(na) && is_const(nb) {
                    continue;
                }
                if allow_alias && (matches!(na, PNode::Alias { .. }) || matches!(nb, PNode::Alias { .. })) {
                    continue;
                }
                if !head_eq(na, nb) {
                    return false;
                }
                let (Ok(ka), Ok(kb)) = (self.kids_of(a), self.kids_of(b)) else {
                    return false;
                };
                if ka.len() != kb.len() {
                    return false;
                }
                if ka.len() > 0 {
                    let Some(slot) = stack.get_mut(sp) else { return false };
                    *slot = Some(Frame { a, b, next_k: 0, depth: d });
                    sp += 1;
                }
                continue;
            }
            let Some(top_idx) = sp.checked_sub(1) else {
                return true;
            };
            let Some(Some(mut frame)) = stack.get(top_idx).copied() else {
                return false;
            };
            let (Ok(ka), Ok(kb)) = (self.kids_of(frame.a), self.kids_of(frame.b)) else {
                return false;
            };
            if frame.next_k < ka.len() {
                let k = frame.next_k;
                frame.next_k = k + 1;
                if let Some(slot) = stack.get_mut(top_idx) {
                    *slot = Some(frame);
                }
                let (Some(x), Some(y)) = (ka.get(k), kb.get(k)) else { return false };
                enter = Some((Side { ty: x, slots: frame.a.slots }, Side { ty: y, slots: frame.b.slots }, frame.depth + 1));
            } else {
                sp = top_idx;
            }
        }
    }

    /// The `k`-th child of `s` FOR AN ERASED COMPARISON: its type, and `s`'s slots kept only
    /// for the table and the environment they name ([`Walk::resolve`], [`Walk::node`],
    /// [`Walk::kids_of`] read nothing else). Never a side whose slots are read as regions.
    fn erased_kid(&self, s: Side<'c>, k: u32) -> Option<Side<'c>> {
        let ty = self.kids_of(s).ok()?.get(k)?;
        Some(Side { ty, slots: s.slots })
    }

    /// `a` (a side, followed through its environment) against `b` (a type of the caller's
    /// table), every region erased: the key a selection row is matched by ([`AskArgs`]).
    fn erased(&self, a: Side<'c>, b: PTy, depth: u32, budget: &Cell<u32>) -> Keyed {
        const MAX_FRAMES: usize = 257; // MOST_WALK_DEPTH (256) + 1
        #[derive(Clone, Copy)]
        struct ErasedFrame<'c> {
            a: Side<'c>,
            b: PTy,
            next_k: u32,
            depth: u32,
        }
        let open = |n: PNode| matches!(n, PNode::Infer | PNode::Error | PNode::Alias { .. });
        let mut stack: [Option<ErasedFrame<'c>>; MAX_FRAMES] = [None; MAX_FRAMES];
        let mut sp: usize = 0;
        let mut out = Keyed::Same;
        let mut enter = Some((a, b, depth));
        loop {
            if let Some((a, b, d)) = enter.take() {
                if d > MOST_WALK_DEPTH || budget.get() == 0 {
                    out = Keyed::Unknown;
                    continue;
                }
                budget.set(budget.get() - 1);
                let a = match self.resolve(a) {
                    Ok(Resolved::Side(s)) => s,
                    Ok(Resolved::Unbound { .. }) | Err(_) => {
                        out = Keyed::Unknown;
                        continue;
                    }
                };
                // One table: a node is its type (regions live in columns, never in nodes).
                if let Slots::Caller(_) = a.slots
                    && a.ty == b
                {
                    continue;
                }
                let (Ok(na), Ok(nb)) = (self.node(a), self.types.node(b)) else {
                    out = Keyed::Unknown;
                    continue;
                };
                // What structure does not decide: a hole, an error, or an alias not normalized here.
                // Distinct `Opaque(x)` / `Opaque(y)` and `Opaque` vs concrete types are distinct
                // nominal types in callers (`head_eq` returns `Keyed::Differ`).
                if open(na) || open(nb) {
                    out = Keyed::Unknown;
                    continue;
                }
                // A const's type is a node of its own table: compare its bits, then its type.
                if let (PNode::ConstValue { ty: ta, bits: x }, PNode::ConstValue { ty: tb, bits: y }) = (na, nb) {
                    if x != y {
                        return Keyed::Differ;
                    }
                    enter = Some((Side { ty: ta, slots: a.slots }, tb, d + 1));
                    continue;
                }
                // An impl's or callee's const parameter (never bound by `blaze-borrow`, since
                // consts carry no region slots and `Array(c, _)` ignores its length) matches a
                // typeck row's const argument loosely (`Keyed::Unknown`), never `Differ`.
                if matches!(a.slots, Slots::Sig { .. }) && matches!(na, PNode::ConstParam(_)) && is_const(nb) {
                    out = Keyed::Unknown;
                    continue;
                }
                if !head_eq(na, nb) {
                    return Keyed::Differ;
                }
                let (Ok(ka), Ok(kb)) = (self.kids_of(a), self.types.kids(b)) else {
                    out = Keyed::Unknown;
                    continue;
                };
                if ka.len() != kb.len() {
                    return Keyed::Differ;
                }
                if ka.len() > 0 {
                    let Some(slot) = stack.get_mut(sp) else {
                        out = Keyed::Unknown;
                        continue;
                    };
                    *slot = Some(ErasedFrame { a, b, next_k: 0, depth: d });
                    sp += 1;
                }
                continue;
            }
            let Some(top_idx) = sp.checked_sub(1) else {
                return out;
            };
            let Some(Some(mut frame)) = stack.get(top_idx).copied() else {
                return Keyed::Unknown;
            };
            let (Ok(ka), Ok(kb)) = (self.kids_of(frame.a), self.types.kids(frame.b)) else {
                out = Keyed::Unknown;
                sp = top_idx;
                continue;
            };
            if frame.next_k < ka.len() {
                let k = frame.next_k;
                frame.next_k = k + 1;
                if let Some(slot) = stack.get_mut(top_idx) {
                    *slot = Some(frame);
                }
                let (Some(x), Some(y)) = (ka.get(k), kb.get(k)) else {
                    out = Keyed::Unknown;
                    sp = top_idx;
                    continue;
                };
                enter = Some((Side { ty: x, slots: frame.a.slots }, y, frame.depth + 1));
            } else {
                sp = top_idx;
            }
        }
    }

    fn len(&self, s: Side<'c>) -> Result<SlotCount, Refused> {
        s.slots.len().ok_or(self.malformed(s.ty, Bad::StepBound))
    }

    /// `s`'s slots are exactly its type's count.
    fn fits(&self, s: Side<'c>) -> Result<SlotCount, Refused> {
        let want = self.tab(s.slots).count(s.ty).map_err(|u| self.count_err(u))?;
        let got = self.len(s)?;
        if want != got {
            return Err(self.malformed(s.ty, Bad::ColumnLength { want, got }));
        }
        Ok(want)
    }

    /// The `k`-th child of `s` and its slots.
    fn kid(&self, s: Side<'c>, kids: &Kids<'_>, rest: Slots<'c>, k: u32) -> Result<(Side<'c>, Slots<'c>), Refused> {
        let ty = kids.get(k).ok_or(self.malformed(s.ty, Bad::NotOfKind))?;
        let n = self.tab(s.slots).count(ty).map_err(|u| self.count_err(u))?;
        let (here, rest) = rest.split(n).ok_or(self.malformed(s.ty, Bad::ColumnLength { want: n, got: rest.len().unwrap_or(SlotCount::ZERO) }))?;
        Ok((Side { ty, slots: here }, rest))
    }

    /// `a` flows where `b` is wanted, under `v`.
    fn walk(&mut self, a: Side<'c>, b: Side<'c>, v: Variance, depth: u32) -> Result<(), Refused> {
        let base_sp = self.walk_sp;
        let saved_bv = self.binder_v;
        if depth == 0 {
            self.binder_v = BinderCtx::None;
        }
        let res = self.walk_loop(a, b, v, depth, base_sp);
        self.walk_sp = base_sp;
        self.binder_v = saved_bv;
        res
    }

    /// `walk` with an explicit enclosing binder context (e.g. builtin `Fn*` calls where the
    /// callable's binder is subtype and the clause's binder is supertype, or `for<..>` clauses).
    fn walk_with_binder(&mut self, a: Side<'c>, b: Side<'c>, v: Variance, bv: BinderCtx, depth: u32) -> Result<(), Refused> {
        let base_sp = self.walk_sp;
        let saved_bv = self.binder_v;
        self.binder_v = bv;
        let res = self.walk_loop(a, b, v, depth, base_sp);
        self.walk_sp = base_sp;
        self.binder_v = saved_bv;
        res
    }

    fn walk_loop(&mut self, a: Side<'c>, b: Side<'c>, v: Variance, depth: u32, base_sp: usize) -> Result<(), Refused> {
        let mut enter = Some((a, b, v, self.binder_v, depth));
        loop {
            if let Some((a, b, v, bv, d)) = enter.take() {
                self.binder_v = bv;
                self.step(a.ty, d)?;
                if d == 0 {
                    // A new relation: its binders are its own.
                    self.bound_len = 0;
                    self.hr_len = 0;
                }
                let (a, b) = match (self.resolve(a)?, self.resolve(b)?) {
                    (Resolved::Side(a), Resolved::Side(b)) => (a, b),
                    (Resolved::Unbound { env, param }, Resolved::Side(other)) | (Resolved::Side(other), Resolved::Unbound { env, param }) => {
                        self.envs.bind_type(env, param, other).map_err(|w| self.malformed(other.ty, w))?;
                        continue;
                    }
                    (Resolved::Unbound { param, .. }, Resolved::Unbound { .. }) => return Err(self.malformed(a.ty, Bad::Unconstrained { index: param })),
                };
                let (na, nb) = (self.node(a)?, self.node(b)?);
                // Two CONST arguments (a value or a const parameter each: `[T; 32]` against `[T; N]`,
                // `Foo<3>` against `Foo<{ N }>` as the callee's head and the caller's instance write
                // them) carry no region (types.rs: no own slot, no child), and the type checker has
                // already proved them equal: the region relation has nothing to say and no shape to
                // check. A const against a TYPE is still `Shape` below (relate tests
                // `a_const_argument_relates_nothing` and its twin).
                if is_const(na) && is_const(nb) {
                    continue;
                }
                // An alias meets something other than the SAME alias (same trait, name, AND erased
                // arguments): normalize it through the impl or bound that proves it at this call.
                // Two aliases for the same trait and name on DIFFERENT subjects (e.g.
                // `<Once as Iterator>::Item` vs `<Iter as Iterator>::Item` in `Iterator::chain`)
                // must normalize rather than equating `Once` with `Iter`.
                let both_alias = matches!((na, nb), (PNode::Alias { .. }, PNode::Alias { .. }));
                let same_alias = matches!((na, nb), (PNode::Alias { trait_: t, name: n, .. }, PNode::Alias { trait_: u, name: m, .. }) if t == u && n == m)
                    && self.erased_sides(a, b);
                if !same_alias {
                    if matches!(na, PNode::Alias { .. }) || matches!(nb, PNode::Alias { .. }) {
                        let mut ba = MOST_WALK_STEPS;
                        let mut bb = MOST_WALK_STEPS;
                        if self.inert(a, 0, &mut ba)? && self.inert(b, 0, &mut bb)? {
                            continue;
                        }
                    }
                    if let PNode::Alias { trait_, name, .. } = na {
                        if self.selected.is_zero_slot_builtin_assoc(trait_, name) {
                            let mut budget = MOST_WALK_STEPS;
                            if self.inert(b, 0, &mut budget)? {
                                continue;
                            }
                        }
                        if both_alias {
                            let saved = (self.bound, self.bound_len, self.hr, self.hr_len, self.binder_v);
                            if let Ok(a_norm) = self.normalize(a, na, d) {
                                enter = Some((a_norm, b, v, bv, d + 1));
                                continue;
                            }
                            (self.bound, self.bound_len, self.hr, self.hr_len, self.binder_v) = saved;
                        } else {
                            let a = self.normalize(a, na, d)?;
                            enter = Some((a, b, v, bv, d + 1));
                            continue;
                        }
                    }
                    if let PNode::Alias { trait_, name, .. } = nb {
                        if self.selected.is_zero_slot_builtin_assoc(trait_, name) {
                            let mut budget = MOST_WALK_STEPS;
                            if self.inert(a, 0, &mut budget)? {
                                continue;
                            }
                        }
                        let b = self.normalize(b, nb, d)?;
                        enter = Some((a, b, v, bv, d + 1));
                        continue;
                    }
                }
                if !head_eq(na, nb) {
                    return Err(Refused::Shape { item: self.item, a: a.ty, b: b.ty, why: ShapeWhy::Heads { a: Head::of(na), b: Head::of(nb) } });
                }
                self.fits(a)?;
                self.fits(b)?;
                let own = self.own(a)?;
                if self.own(b)? != own {
                    return Err(Refused::Shape { item: self.item, a: a.ty, b: b.ty, why: ShapeWhy::OwnCount });
                }
                let ta = self.tab(a.slots);
                for k in 0..own.get() {
                    let vk = v.then(ta.variance(a.ty, Pos::Own(k)).map_err(|u| self.count_err(u))?);
                    let (ra, rb) = (self.slot(a, k)?, self.slot(b, k)?);
                    self.pair(a.ty, ra, rb, vk, Self::env_of(a), Self::env_of(b))?;
                }
                let (ka, kb) = (self.kids_of(a)?, self.kids_of(b)?);
                if ka.len() != kb.len() {
                    return Err(Refused::Shape { item: self.item, a: a.ty, b: b.ty, why: ShapeWhy::KidCount });
                }
                let (_, ra) = a.slots.split(own).ok_or(self.malformed(a.ty, Bad::StepBound))?;
                let (_, rb) = b.slots.split(own).ok_or(self.malformed(b.ty, Bad::StepBound))?;
                let child_bv = match na {
                    PNode::FnPtr(..) => BinderCtx::FnPtr(v),
                    PNode::Dynamic(_) => BinderCtx::Dynamic(v),
                    _ => bv,
                };
                if ka.len() > 0 {
                    let Some(slot) = self.walk_stack.get_mut(self.walk_sp) else {
                        return Err(self.malformed(a.ty, Bad::StepBound));
                    };
                    *slot = Some(WalkFrame { a_ty: a.ty, b_ty: b.ty, ra, rb, v, binder_v: child_bv, next_k: 0, depth: d });
                    self.walk_sp += 1;
                }
                continue;
            }
            if self.walk_sp <= base_sp {
                return Ok(());
            }
            let top_idx = self.walk_sp - 1;
            let Some(Some(mut frame)) = self.walk_stack.get(top_idx).copied() else {
                return Err(self.malformed(a.ty, Bad::StepBound));
            };
            let fa = Side { ty: frame.a_ty, slots: frame.ra };
            let fb = Side { ty: frame.b_ty, slots: frame.rb };
            let (ka, kb) = (self.kids_of(fa)?, self.kids_of(fb)?);
            if frame.next_k < ka.len() {
                let k = frame.next_k;
                let ta = self.tab(fa.slots);
                let vk = frame.v.then(ta.variance(fa.ty, Pos::Kid(k)).map_err(|u| self.count_err(u))?);
                let (ca, na_) = self.kid(fa, &ka, frame.ra, k)?;
                let (cb, nb_) = self.kid(fb, &kb, frame.rb, k)?;
                frame.ra = na_;
                frame.rb = nb_;
                frame.next_k = k + 1;
                if let Some(slot) = self.walk_stack.get_mut(top_idx) {
                    *slot = Some(frame);
                }
                enter = Some((ca, cb, vk, frame.binder_v, frame.depth + 1));
            } else {
                self.walk_sp = top_idx;
            }
        }
    }

    /// Normalize an outer `PNode::Alias` subject when possible (e.g. `<<I as Iterator>::Item as
    /// IntoIterator>::IntoIter` -> `option::IterMut`), keeping `s` unchanged if it is rigid.
    fn try_normalize_subject(&mut self, mut s: Side<'c>, depth: u32) -> Side<'c> {
        if depth >= MOST_WALK_DEPTH / 2 {
            return s;
        }
        for step in 0..8u32 {
            let Ok(Resolved::Side(cur)) = self.resolve(s) else { break };
            s = cur;
            let Ok(n @ PNode::Alias { .. }) = self.node(s) else { break };
            let saved = (self.bound, self.bound_len, self.hr, self.hr_len, self.binder_v);
            match self.normalize(s, n, depth + step + 1) {
                Ok(next) => {
                    (self.bound, self.bound_len, self.hr, self.hr_len, self.binder_v) = saved;
                    s = next;
                }
                Err(_) => {
                    (self.bound, self.bound_len, self.hr, self.hr_len, self.binder_v) = saved;
                    break;
                }
            }
        }
        s
    }

    /// The selected impl's (or bound's / builtin callable's) value for the alias `s` (its node `n`).
    fn normalize(&mut self, s: Side<'c>, n: PNode, depth: u32) -> Result<Side<'c>, Refused> {
        let PNode::Alias { trait_, name, .. } = n else { return Err(self.malformed(s.ty, Bad::NotOfKind)) };
        self.fits(s)?;
        let own = self.own(s)?;
        let kids = self.kids_of(s)?;
        let (_, args_rest) = s.slots.split(own).ok_or(self.malformed(s.ty, Bad::StepBound))?;
        let mut norm_subject = None;
        if kids.len() > 0 {
            let (subject, _) = self.kid(s, &kids, args_rest, 0)?;
            if let Resolved::Side(subject) = self.resolve(subject)? {
                let subject = self.try_normalize_subject(subject, depth);
                norm_subject = Some(subject);
                if let Some(val) = self.try_normalize_non_impl(s, kids.len(), subject, trait_, name)? {
                    return Ok(val);
                }
            }
        }
        let selected: &'c dyn Selected = self.selected;
        let at = match norm_subject {
            Some(subject) => AskAt::SubjectKids { subject, of: s, len: kids.len() },
            None => AskAt::Kids(s),
        };
        let key = Keyer { walk: &*self, at, budget: Cell::new(MOST_WALK_STEPS) };
        let (head, value) = match selected.select_keyed(Ask { trait_, name: Some(name), written: s.ty }, &key) {
            Selection::Impl { head, value: Some(value) } => (head, value),
            Selection::Impl { value: None, .. } => return Err(self.malformed(s.ty, Bad::NoValue)),
            Selection::Not(why) => return Err(Refused::Unselected { item: self.item, node: s.ty, trait_, name: Some(name), why }),
        };
        let ImplValue { value, gat_lifetimes } = value;
        let env = self.envs.impl_env_gat(head.lifetimes, gat_lifetimes, head.types).ok_or(self.malformed(s.ty, Bad::StepBound))?;
        // The header subject-first ([`ImplHead::trait_ref`]), as the alias is: `<S as Tr<A..>>`'s
        // children `[S, A..]` meet the header's `[SelfTy, A'..]` one for one.
        let header = Side { ty: head.trait_ref.ty, slots: Slots::Sig { col: head.trait_ref.col, env } };
        let tl = self.own(header)?;
        if tl > own || own.get() - tl.get() != gat_lifetimes {
            return Err(self.malformed(s.ty, Bad::ColumnLength { want: own, got: tl }));
        }
        self.fits(header)?;
        // The trait's lifetime arguments: the alias's first own slots against the header's.
        for k in 0..tl.get() {
            let (x, y) = (self.slot(header, k)?, self.slot(s, k)?);
            self.pair(s.ty, x, y, Variance::In, Self::env_of(header), Self::env_of(s))?;
        }
        // A GAT's own lifetimes: numbered after the impl's.
        for j in 0..gat_lifetimes {
            match self.slot(s, tl.get() + j)? {
                RegAt::Known(r) => self.envs.bind_reg(env, head.lifetimes + j, r).map_err(|w| self.malformed(s.ty, w))?,
                RegAt::Unbound { param, .. } => return Err(self.malformed(s.ty, Bad::Unconstrained { index: param })),
            }
        }
        // `Self`, then the trait's type arguments: the header's children against the alias's.
        let kids = self.kids_of(s)?;
        let hkids = self.kids_of(header)?;
        if kids.len() != hkids.len() {
            return Err(self.malformed(s.ty, Bad::HeaderKids { alias: kids.len(), header: hkids.len() }));
        }
        let (_, mut rest) = s.slots.split(own).ok_or(self.malformed(s.ty, Bad::StepBound))?;
        let (_, mut hrest) = header.slots.split(tl).ok_or(self.malformed(s.ty, Bad::StepBound))?;
        for k in 0..hkids.len() {
            let (h, hr) = self.kid(header, &hkids, hrest, k)?;
            let (x, r) = self.kid(s, &kids, rest, k)?;
            hrest = hr;
            rest = r;
            self.walk(h, x, Variance::In, depth + 1)?;
        }
        self.impl_where(head.imp, trait_, s.ty, env, head.lifetimes)?;
        Ok(Side { ty: value.ty, slots: Slots::Sig { col: value.col, env } })
    }

    /// Every component of `s` outlives `region`.
    fn components(&mut self, s: Side<'c>, region: Reg, depth: u32) -> Result<(), Refused> {
        let base_sp = self.comp_sp;
        let res = self.components_loop(s, region, depth, base_sp);
        self.comp_sp = base_sp;
        res
    }

    fn components_loop(&mut self, s: Side<'c>, region: Reg, depth: u32, base_sp: usize) -> Result<(), Refused> {
        let mut enter = Some((s, depth));
        loop {
            if let Some((cur, d)) = enter.take() {
                self.step(cur.ty, d)?;
                let cur = match self.resolve(cur)? {
                    Resolved::Side(s) => s,
                    Resolved::Unbound { param, .. } => return Err(self.malformed(cur.ty, Bad::Unconstrained { index: param })),
                };
                let n = self.node(cur)?;
                match n {
                    PNode::Param(p) => {
                        // The caller's own type parameter: `ρ_P: 'r`, checked against the caller's bounds.
                        let rho = self.owner.param(p).ok_or(self.malformed(cur.ty, Bad::NoParamOrigin { param: p }))?;
                        self.outlives(cur.ty, Reg::Origin(rho), region)?;
                        continue;
                    }
                    PNode::Alias { trait_, name, .. } => {
                        let selected: &'c dyn Selected = self.selected;
                        let key = Keyer { walk: &*self, at: AskAt::Kids(cur), budget: Cell::new(MOST_WALK_STEPS) };
                        if let Selection::Impl { value: Some(_), .. } = selected.select_keyed(Ask { trait_, name: Some(name), written: cur.ty }, &key) {
                            let value = self.normalize(cur, n, d)?;
                            enter = Some((value, d + 1));
                            continue;
                        }
                        // Not selected: the alias outlives `'r` if its arguments do (RFC 1214's
                        // projection rule, its sufficient half).
                    }
                    PNode::Closure { .. } | PNode::Future(..) if matches!(cur.slots, Slots::Caller(_)) => {
                        // A body type of the caller: every type parameter its captures hold outlives
                        // `'r` (the Reference, "Closure types"; `spawn(|| use_p(p))` with `F: 'static`
                        // asks `P: 'static`, E0310 where the caller has no such bound). Its own slots
                        // below. Census wave-1119 (b): `(Param(1), Uni(Static))` at `spawn_blocking`.
                        let owner: &'c dyn Owner = self.owner;
                        let params = owner.body_params(cur.ty).ok_or(self.malformed(cur.ty, Bad::BodyParamsUnread))?;
                        for &p in params {
                            let rho = owner.param(p).ok_or(self.malformed(cur.ty, Bad::NoParamOrigin { param: p }))?;
                            self.outlives(cur.ty, Reg::Origin(rho), region)?;
                        }
                    }
                    _ => {}
                }
                self.fits(cur)?;
                let own = self.own(cur)?;
                for k in 0..own.get() {
                    match self.slot(cur, k)? {
                        RegAt::Known(Reg::Bound { .. }) => {}
                        RegAt::Known(r) => self.outlives(cur.ty, r, region)?,
                        RegAt::Unbound { param, .. } => return Err(self.malformed(cur.ty, Bad::Unconstrained { index: param })),
                    }
                }
                if let PNode::Dynamic(_) = n {
                    // `dyn Tr + 'o: 'r` iff `'o: 'r`.
                    continue;
                }
                let kids = self.kids_of(cur)?;
                let (_, rest) = cur.slots.split(own).ok_or(self.malformed(cur.ty, Bad::StepBound))?;
                if kids.len() > 0 {
                    let Some(slot) = self.comp_stack.get_mut(self.comp_sp) else {
                        return Err(self.malformed(cur.ty, Bad::StepBound));
                    };
                    *slot = Some(CompFrame { ty: cur.ty, rest, next_k: 0, depth: d });
                    self.comp_sp += 1;
                }
                continue;
            }
            if self.comp_sp <= base_sp {
                return Ok(());
            }
            let top_idx = self.comp_sp - 1;
            let Some(Some(mut frame)) = self.comp_stack.get(top_idx).copied() else {
                return Err(self.malformed(s.ty, Bad::StepBound));
            };
            let fs = Side { ty: frame.ty, slots: frame.rest };
            let kids = self.kids_of(fs)?;
            if frame.next_k < kids.len() {
                let k = frame.next_k;
                let (c, r) = self.kid(fs, &kids, frame.rest, k)?;
                frame.rest = r;
                frame.next_k = k + 1;
                if let Some(slot) = self.comp_stack.get_mut(top_idx) {
                    *slot = Some(frame);
                }
                enter = Some((c, frame.depth + 1));
            } else {
                self.comp_sp = top_idx;
            }
        }
    }

    /// The well-formedness of an instantiated signature type at a call (RFC 1214, `WF(&'r T)`
    /// requires `T: 'r`): for each reference `&'r T` in `s`, every component of `T` outlives
    /// `'r`. Descends through references, raw pointers, slices, arrays, pattern types, tuples
    /// and ADTs only: a fn pointer's types are its binder's (their WF is the pointer's own
    /// check), an alias, an opaque type, a trait object and a body type are taken as given (their
    /// well-formedness is the type checker's). A reference whose region a binder inside the
    /// signature binds (`Reg::Bound`) adds nothing here.
    fn wf(&mut self, s: Side<'c>, depth: u32) -> Result<(), Refused> {
        const MAX_FRAMES: usize = 257; // MOST_WALK_DEPTH (256) + 1
        #[derive(Clone, Copy)]
        struct WfFrame<'c> {
            s: Side<'c>,
            is_ref: bool,
            rest: Slots<'c>,
            next_k: u32,
            depth: u32,
        }
        let mut stack: [Option<WfFrame<'c>>; MAX_FRAMES] = [None; MAX_FRAMES];
        let mut sp: usize = 0;
        let mut enter = Some((s, depth));
        loop {
            if let Some((cur, d)) = enter.take() {
                self.step(cur.ty, d)?;
                let cur = match self.resolve(cur)? {
                    Resolved::Side(s) => s,
                    Resolved::Unbound { param, .. } => return Err(self.malformed(cur.ty, Bad::Unconstrained { index: param })),
                };
                let n = self.node(cur)?;
                let descend = matches!(n, PNode::Ref(..) | PNode::Ptr(..) | PNode::Slice(_) | PNode::Array(..) | PNode::Pat(..) | PNode::Tuple(_) | PNode::Record(..) | PNode::Union(_) | PNode::Adt(..));
                if !descend {
                    continue;
                }
                self.fits(cur)?;
                let own = self.own(cur)?;
                let kids = self.kids_of(cur)?;
                let (_, rest) = cur.slots.split(own).ok_or(self.malformed(cur.ty, Bad::StepBound))?;
                if kids.len() > 0 {
                    let Some(slot) = stack.get_mut(sp) else {
                        return Err(self.malformed(cur.ty, Bad::StepBound));
                    };
                    *slot = Some(WfFrame { s: cur, is_ref: matches!(n, PNode::Ref(..)), rest, next_k: 0, depth: d });
                    sp += 1;
                }
                continue;
            }
            let Some(top_idx) = sp.checked_sub(1) else {
                return Ok(());
            };
            let Some(Some(mut frame)) = stack.get(top_idx).copied() else {
                return Err(self.malformed(s.ty, Bad::StepBound));
            };
            let kids = self.kids_of(frame.s)?;
            if frame.next_k < kids.len() {
                let k = frame.next_k;
                let (c, r) = self.kid(frame.s, &kids, frame.rest, k)?;
                frame.rest = r;
                frame.next_k = k + 1;
                if let Some(slot) = stack.get_mut(top_idx) {
                    *slot = Some(frame);
                }
                if frame.is_ref {
                    match self.slot(frame.s, 0)? {
                        RegAt::Known(Reg::Bound { .. }) => {}
                        RegAt::Known(region) => self.components(c, region, frame.depth + 1)?,
                        RegAt::Unbound { param, .. } => return Err(self.malformed(frame.s.ty, Bad::Unconstrained { index: param })),
                    }
                }
                enter = Some((c, frame.depth + 1));
            } else {
                sp = top_idx;
            }
        }
    }

    /// `subject: trait_ref`: its regions tied to what proves it.
    ///
    /// ONE relation (as [`Walk::tie_of`]): its walks share the alpha pairs of the clause's
    /// binder, so each runs at depth 1 after one reset here.
    fn tie(&mut self, subject: Side<'c>, trait_ref: Side<'c>) -> Result<(), Refused> {
        let PNode::ExTrait(trait_, _) = self.node(trait_ref)? else { return Err(self.malformed(trait_ref.ty, Bad::NotOfKind)) };
        self.bound_len = 0;
        self.hr_len = 0;
        let subject = match self.resolve(subject)? {
            Resolved::Side(s) => s,
            Resolved::Unbound { param, .. } => return Err(self.malformed(subject.ty, Bad::Unconstrained { index: param })),
        };
        if self.try_prove_trait_non_impl(subject, trait_ref, false, trait_)? {
            return Ok(());
        }
        let param_miss = matches!(self.node(subject)?, PNode::Param(_));
        let selected: &'c dyn Selected = self.selected;
        let key = Keyer { walk: &*self, at: AskAt::Subject { subject, of: trait_ref }, budget: Cell::new(MOST_WALK_STEPS) };
        match selected.select_keyed(Ask { trait_, name: None, written: trait_ref.ty }, &key) {
            Selection::Impl { head, .. } => {
                let env = self.envs.impl_env(head.lifetimes, head.types).ok_or(self.malformed(trait_ref.ty, Bad::StepBound))?;
                // The header subject-first ([`ImplHead::trait_ref`]); `trait_ref` here holds no
                // subject: the header's first child meets `subject`, the rest `trait_ref`'s.
                let header = Side { ty: head.trait_ref.ty, slots: Slots::Sig { col: head.trait_ref.col, env } };
                self.fits(header)?;
                self.fits(trait_ref)?;
                let (tl, own) = (self.own(header)?, self.own(trait_ref)?);
                let (hkids, kids) = (self.kids_of(header)?, self.kids_of(trait_ref)?);
                if tl != own || hkids.len() != kids.len() + 1 {
                    return Err(Refused::Shape { item: self.item, a: header.ty, b: trait_ref.ty, why: ShapeWhy::ImplHeader });
                }
                for k in 0..own.get() {
                    let (x, y) = (self.slot(header, k)?, self.slot(trait_ref, k)?);
                    self.pair(trait_ref.ty, x, y, Variance::In, Self::env_of(header), Self::env_of(trait_ref))?;
                }
                let (_, hrest) = header.slots.split(tl).ok_or(self.malformed(header.ty, Bad::StepBound))?;
                let (h0, mut hrest) = self.kid(header, &hkids, hrest, 0)?;
                self.walk(h0, subject, Variance::In, 1)?;
                let (_, mut rest) = trait_ref.slots.split(own).ok_or(self.malformed(trait_ref.ty, Bad::StepBound))?;
                for k in 0..kids.len() {
                    let (h, hr) = self.kid(header, &hkids, hrest, k + 1)?;
                    let (x, r) = self.kid(trait_ref, &kids, rest, k)?;
                    hrest = hr;
                    rest = r;
                    self.walk(h, x, Variance::In, 1)?;
                }
                self.impl_where(head.imp, trait_, trait_ref.ty, env, head.lifetimes)
            }
            Selection::Not(NotSelected::Builtin) if self.nothing_to_tie(trait_ref, 0)? => Ok(()),
            Selection::Not(NotSelected::Unanswered | NotSelected::Generic | NotSelected::GenericSelf(_)) if param_miss => {
                Err(Refused::Unselected { item: self.item, node: trait_ref.ty, trait_, name: None, why: NotSelected::NoBound })
            }
            Selection::Not(why) => Err(Refused::Unselected { item: self.item, node: trait_ref.ty, trait_, name: None, why }),
        }
    }

    /// Whether the trait reference `trait_ref` names no region and no type past its first
    /// `subjects` children (its subject, when laid subject-first): a builtin rule's proof of it
    /// then has nothing to tie. A builtin rule (`Sized`; `Copy` and `Clone` of a tuple, an
    /// array, a fn pointer, a closure) puts no region requirement on its subject (the
    /// Reference, "Special types and traits"), and a trait without parameters has no lifetime
    /// for a proof to bind. A trait with a parameter (the `Fn` family's arguments, `Unsize<U>`)
    /// is still refused by name.
    fn nothing_to_tie(&self, trait_ref: Side<'c>, subjects: u32) -> Result<bool, Refused> {
        let own = self.own(trait_ref)?;
        let kids = self.kids_of(trait_ref)?;
        Ok(own == SlotCount::ZERO && kids.len() == subjects)
    }

    /// Region `r` of a column read in `env`, every parameter bound.
    fn known_in(&self, env: EnvId, r: Region, node: PTy) -> Result<Reg, Refused> {
        match self.envs.reg(env, r).map_err(|w| self.malformed(node, w))? {
            RegAt::Known(reg) => Ok(reg),
            RegAt::Unbound { param, .. } => Err(self.malformed(node, Bad::Unconstrained { index: param })),
        }
    }

    /// THE IMPL'S PROOF. The selected impl `imp` (of `trait_`, `lifetimes` lifetime
    /// parameters) proves what it was selected for only where its where-clauses hold (the
    /// Reference, "Implementations": an impl's where-clauses must hold where it is used; RFC
    /// 1214 for `T: 'r`), so they are related at the call like a callee's clauses, read in the
    /// impl's environment `env` once its header walk bound what the header names: each `'a: 'b`
    /// a subset, each `T: 'r` the components rule, each `T: Tr<..>` tied ([`Walk::tie_of`],
    /// nesting bounded by [`MOST_IMPL_NESTING`]). An impl lifetime the header does not name
    /// (`impl<'a, T: Tr<'a>> Other for T`) is the proof's choice: a fresh existential origin.
    /// A projection-equality clause has no rule yet: refused by name
    /// ([`NotSelected::WhereProjection`]). The relation's alpha pairs are kept across.
    fn impl_where(&mut self, imp: ItemKey, trait_: ItemKey, node: PTy, env: EnvId, lifetimes: u32) -> Result<(), Refused> {
        let selected: &'c dyn Selected = self.selected;
        let w = selected.impl_where(imp).map_err(|why| Refused::Unselected { item: self.item, node, trait_, name: None, why })?;
        if w.params.is_empty() && w.outlives.is_empty() && w.views.is_empty() {
            return Ok(());
        }
        if self.nested >= MOST_IMPL_NESTING {
            return Err(self.malformed(node, Bad::StepBound));
        }
        for i in 0..lifetimes {
            if let RegAt::Unbound { param, .. } = self.envs.reg(env, Region::Param(i)).map_err(|w| self.malformed(node, w))? {
                let o = self.out.fresh(Kind::Existential);
                self.envs.bind_reg(env, param, Reg::Origin(o)).map_err(|w| self.malformed(node, w))?;
            }
        }
        let saved = (self.bound, self.bound_len, self.hr, self.hr_len, self.binder_v);
        self.nested += 1;
        let res = self.impl_where_in(w, trait_, node, env);
        self.nested -= 1;
        (self.bound, self.bound_len, self.hr, self.hr_len, self.binder_v) = saved;
        res
    }

    fn impl_where_in(&mut self, w: crate::inst::ImplWhere<'c>, trait_: ItemKey, node: PTy, env: EnvId) -> Result<(), Refused> {
        // Projection clauses FIRST: an impl type parameter that only appears in a projection
        // where-clause (`impl<'a, I, T: 'a + Clone> Iterator for Cloned<I> where I: Iterator<Item = &'a T>`)
        // is bound by `proj_of` before `T: 'a` and `T: Clone` read `T`.
        for cv in w.views {
            let subject = w.outlives.iter().any(|o| matches!(*o, Outlives::Type { clause, .. } if clause == cv.nth));
            if subject {
                continue;
            }
            let side = Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env } };
            if matches!(self.node(side)?, PNode::ExProj { .. }) {
                self.proj_of(side)?;
            }
        }
        for &(long, short) in w.params {
            let (l, s) = (self.known_in(env, long, node)?, self.known_in(env, short, node)?);
            self.outlives(node, l, s)?;
        }
        for o in w.outlives {
            match *o {
                Outlives::Region { long, short } => {
                    if w.params.contains(&(long, short)) {
                        continue;
                    }
                    let (l, s) = (self.known_in(env, long, node)?, self.known_in(env, short, node)?);
                    self.outlives(node, l, s)?;
                }
                Outlives::Type { clause, region } => {
                    let Some(cv) = w.views.iter().find(|c| c.nth == clause) else {
                        return Err(Refused::Unselected { item: self.item, node, trait_, name: None, why: NotSelected::HeadUnread });
                    };
                    let r = self.known_in(env, region, node)?;
                    self.components(Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env } }, r, 0)?;
                }
            }
        }
        // Every trait clause, its column empty or not: one that names no region of
        // the call relates nothing ([`Walk::inert`], asked by `tie_of`), any other
        // is tied or refused by name. Skipping a clause for its EMPTY COLUMN alone was looser
        // than Rust: `impl<T: Marker> Other for Wrap<T>` (`T: Marker` writes no region) with
        // `impl Marker for &'static u8` proves `Wrap<&'x u8>: Other` only where `'x: 'static`
        // (tests.rs `an_impls_empty_column_clause_is_tied_where_it_names_a_region`).
        for cv in w.views {
            let subject = w.outlives.iter().any(|o| matches!(*o, Outlives::Type { clause, .. } if clause == cv.nth));
            if subject {
                continue;
            }
            let side = Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env } };
            if matches!(self.node(side)?, PNode::ExTrait(..)) {
                self.tie_of(side)?;
            }
        }
        Ok(())
    }

    /// THE EMPTY-COLUMN RULE: whether `s` (a clause, or a part of one, followed through its
    /// environment) names NO region of the call: no slot anywhere under it, no type parameter of
    /// the caller (its `ρ_P`), no body type of the caller holding one, no impl parameter left to
    /// the proof's choice.
    ///
    /// Rust's rule (the Reference, "Trait and lifetime bounds", "Implementations": a where-clause
    /// must hold for the instance the use makes; rustc confirms the selected impl by equating its
    /// header with that instance and proves its where-clauses there): the proof of `T: Tr<A..>`
    /// (an impl, a builtin rule, the caller's bound) can relate only the regions its instance
    /// names (each region of `T` and `A..`, each `ρ_P` of a caller parameter they hold, a body
    /// type's captures). A clause naming none relates nothing: skipped EXACTLY. Every other
    /// clause is tied or refused by name, never skipped, whatever its column holds: a column is
    /// empty where the clause WRITES no region (`T: Marker`, `I: Iterator<Item = T>`, `F:
    /// FnOnce(T) -> U`), while the instance of `T` may hold any.
    fn inert(&self, s: Side<'c>, depth: u32, budget: &mut u32) -> Result<bool, Refused> {
        const MAX_FRAMES: usize = 257; // MOST_WALK_DEPTH (256) + 1
        #[derive(Clone, Copy)]
        struct InertFrame<'c> {
            s: Side<'c>,
            rest: Slots<'c>,
            next_k: u32,
            depth: u32,
        }
        let mut stack: [Option<InertFrame<'c>>; MAX_FRAMES] = [None; MAX_FRAMES];
        let mut sp: usize = 0;
        let mut enter = Some((s, depth));
        loop {
            if let Some((cur, d)) = enter.take() {
                if d > MOST_WALK_DEPTH || *budget == 0 {
                    return Err(self.malformed(cur.ty, Bad::StepBound));
                }
                *budget -= 1;
                let cur = match self.resolve(cur)? {
                    Resolved::Side(s) => s,
                    // An impl parameter no header walk bound: the proof's choice, which may be anything.
                    Resolved::Unbound { .. } => return Ok(false),
                };
                if self.fits(cur)? != SlotCount::ZERO {
                    return Ok(false);
                }
                match self.node(cur)? {
                    // The caller's own parameter (its `ρ_P`), or a type not decided.
                    PNode::Param(_) | PNode::Infer | PNode::Error => return Ok(false),
                    PNode::Closure { .. } | PNode::Future(..) | PNode::Coroutine { .. } => {
                        // A body type of the caller with no slot: inert only where its captures hold no
                        // caller parameter (`Owner::body_params`, the components rule's answer). A body
                        // type in a declared column is never laid there: not inert.
                        let Slots::Caller(_) = cur.slots else { return Ok(false) };
                        let owner: &'c dyn Owner = self.owner;
                        if !matches!(owner.body_params(cur.ty), Some(p) if p.is_empty()) {
                            return Ok(false);
                        }
                        continue;
                    }
                    _ => {}
                }
                // No slot under `cur` (`fits` above): each child takes none of `rest`.
                let kids = self.kids_of(cur)?;
                if kids.len() > 0 {
                    let Some(slot) = stack.get_mut(sp) else {
                        return Err(self.malformed(cur.ty, Bad::StepBound));
                    };
                    *slot = Some(InertFrame { s: cur, rest: cur.slots, next_k: 0, depth: d });
                    sp += 1;
                }
                continue;
            }
            let Some(top_idx) = sp.checked_sub(1) else {
                return Ok(true);
            };
            let Some(Some(mut frame)) = stack.get(top_idx).copied() else {
                return Err(self.malformed(s.ty, Bad::StepBound));
            };
            let kids = self.kids_of(frame.s)?;
            if frame.next_k < kids.len() {
                let k = frame.next_k;
                let (c, r) = self.kid(frame.s, &kids, frame.rest, k)?;
                frame.rest = r;
                frame.next_k = k + 1;
                if let Some(slot) = stack.get_mut(top_idx) {
                    *slot = Some(frame);
                }
                enter = Some((c, frame.depth + 1));
            } else {
                sp = top_idx;
            }
        }
    }

    /// `<T as Tr<A..>>::N == U` (an `ExProj` laid subject-first, `[T, A..]` then the term `U`).
    /// Rust's rule (the Reference, "Associated types"; rustc's projection predicate): the
    /// alias, normalized through what proves `T: Tr<A..>`, EQUALS `U`. Where `U` names no
    /// region of the call ([`Walk::inert`]), the normalized value equals a type with none, so
    /// the equation relates nothing; what proving `T: Tr<A..>` relates is its parent trait
    /// clause's (laid apart, tied by [`Walk::tie_of`]). Any other projection clause has no rule
    /// here yet: refused by name ([`NotSelected::WhereProjection`]), never skipped
    /// (`I: Iterator<Item = T>` with `I = Iter<'x, u8>` makes `T`'s region `'x`).
    fn proj_of(&mut self, proj: Side<'c>) -> Result<(), Refused> {
        let PNode::ExProj { trait_, name, .. } = self.node(proj)? else { return Err(self.malformed(proj.ty, Bad::NotOfKind)) };
        self.fits(proj)?;
        let own = self.own(proj)?;
        let kids = self.kids_of(proj)?;
        let Some(last) = kids.len().checked_sub(1) else { return Err(self.malformed(proj.ty, Bad::NotOfKind)) };
        let (_, args_rest) = proj.slots.split(own).ok_or(self.malformed(proj.ty, Bad::StepBound))?;
        let mut rest = args_rest;
        let mut subject = None;
        let mut term = None;
        for k in 0..kids.len() {
            let (c, r) = self.kid(proj, &kids, rest, k)?;
            rest = r;
            if k == 0 {
                subject = Some(c);
            }
            if k == last {
                term = Some(c);
            }
        }
        let subject = subject.ok_or(self.malformed(proj.ty, Bad::NotOfKind))?;
        let term = term.ok_or(self.malformed(proj.ty, Bad::NotOfKind))?;
        let mut budget = MOST_WALK_STEPS;
        if self.inert(term, 0, &mut budget)? {
            return Ok(());
        }
        if self.selected.is_zero_slot_builtin_assoc(trait_, name) && self.fits(term)? == SlotCount::ZERO {
            return Ok(());
        }
        let subject = match self.resolve(subject)? {
            Resolved::Side(s) => self.try_normalize_subject(s, 0),
            Resolved::Unbound { param, .. } => return Err(self.malformed(subject.ty, Bad::Unconstrained { index: param })),
        };
        self.bound_len = 0;
        self.hr_len = 0;
        // Builtin `FnOnce::Output` on `FnDef`, `FnPtr`, or caller `Closure`.
        if self.selected.is_fn_trait(trait_) && last == 2 {
            let (_, mut prest) = proj.slots.split(own).ok_or(self.malformed(proj.ty, Bad::StepBound))?;
            let (_, r0) = self.kid(proj, &kids, prest, 0)?;
            prest = r0;
            let (args_tuple, _) = self.kid(proj, &kids, prest, 1)?;
            if self.builtin_fn_trait(subject, args_tuple, Some(term), trait_, proj.ty)?.is_some() {
                return Ok(());
            }
        }
        if let Some(norm) = self.try_normalize_non_impl(proj, last, subject, trait_, name)? {
            return self.walk(norm, term, Variance::In, 1);
        }
        let selected: &'c dyn Selected = self.selected;
        let key = Keyer { walk: &*self, at: AskAt::SubjectKids { subject, of: proj, len: last }, budget: Cell::new(MOST_WALK_STEPS) };
        let (head, value) = match selected.select_keyed(Ask { trait_, name: Some(name), written: proj.ty }, &key) {
            Selection::Impl { head, value: Some(value) } => (head, value),
            Selection::Impl { value: None, .. } => return Err(self.malformed(proj.ty, Bad::NoValue)),
            Selection::Not(why) => return Err(Refused::Unselected { item: self.item, node: proj.ty, trait_, name: Some(name), why }),
        };
        let ImplValue { value, gat_lifetimes } = value;
        let env = self.envs.impl_env_gat(head.lifetimes, gat_lifetimes, head.types).ok_or(self.malformed(proj.ty, Bad::StepBound))?;
        let header = Side { ty: head.trait_ref.ty, slots: Slots::Sig { col: head.trait_ref.col, env } };
        let tl = self.own(header)?;
        if tl > own || own.get() - tl.get() != gat_lifetimes {
            return Err(self.malformed(proj.ty, Bad::ColumnLength { want: own, got: tl }));
        }
        self.fits(header)?;
        self.bound_len = 0;
        self.hr_len = 0;
        for k in 0..tl.get() {
            let (x, y) = (self.slot(header, k)?, self.slot(proj, k)?);
            self.pair(proj.ty, x, y, Variance::In, Self::env_of(header), Self::env_of(proj))?;
        }
        for j in 0..gat_lifetimes {
            match self.slot(proj, tl.get() + j)? {
                RegAt::Known(r) => self.envs.bind_reg(env, head.lifetimes + j, r).map_err(|w| self.malformed(proj.ty, w))?,
                RegAt::Unbound { param, .. } => return Err(self.malformed(proj.ty, Bad::Unconstrained { index: param })),
            }
        }
        let hkids = self.kids_of(header)?;
        if last != hkids.len() {
            return Err(self.malformed(proj.ty, Bad::HeaderKids { alias: last, header: hkids.len() }));
        }
        let (_, mut hrest) = header.slots.split(tl).ok_or(self.malformed(header.ty, Bad::StepBound))?;
        let mut prest = args_rest;
        for k in 0..hkids.len() {
            let (h, hr) = self.kid(header, &hkids, hrest, k)?;
            let (x, r) = self.kid(proj, &kids, prest, k)?;
            hrest = hr;
            prest = r;
            self.walk(h, x, Variance::In, 1)?;
        }
        self.impl_where(head.imp, trait_, proj.ty, env, head.lifetimes)?;
        let norm = Side { ty: value.ty, slots: Slots::Sig { col: value.col, env } };
        self.walk(norm, term, Variance::In, 1)
    }

    /// `T: Trait<A..>` laid subject-first (`ExTrait(Trait, [T, A..])`, a clause column's form):
    /// as [`Walk::tie`], with the subject read off the node.
    fn tie_of(&mut self, trait_ref: Side<'c>) -> Result<(), Refused> {
        let PNode::ExTrait(trait_, _) = self.node(trait_ref)? else { return Err(self.malformed(trait_ref.ty, Bad::NotOfKind)) };
        let mut budget = MOST_WALK_STEPS;
        if self.inert(trait_ref, 0, &mut budget)? {
            return Ok(());
        }
        // `F: [const] Destruct`, `T: Sized`: no proof of it relates a region (no impl can be
        // written; the builtin rule reads none), whatever the instance holds.
        let selected: &'c dyn Selected = self.selected;
        if selected.relates_nothing(trait_) {
            return Ok(());
        }
        self.bound_len = 0;
        self.hr_len = 0;
        self.fits(trait_ref)?;
        let own = self.own(trait_ref)?;
        let kids = self.kids_of(trait_ref)?;
        let (_, rest) = trait_ref.slots.split(own).ok_or(self.malformed(trait_ref.ty, Bad::StepBound))?;
        let (subject, mut rest) = self.kid(trait_ref, &kids, rest, 0)?;
        let subject = match self.resolve(subject)? {
            Resolved::Side(s) => self.try_normalize_subject(s, 0),
            Resolved::Unbound { param, .. } => return Err(self.malformed(subject.ty, Bad::Unconstrained { index: param })),
        };
        // If normalizing the subject or any alias trait argument yields an all-inert goal
        // (e.g. `usize: Sum<<Map<Iter<'a, _>, C> as Iterator>::Item>` -> `usize: Sum<usize>`),
        // the normalized impls' own where-clauses have already been checked and the goal itself
        // relates no region (so `selected_rows` rightly skipped filing a row for it).
        if own == SlotCount::ZERO {
            let mut b = MOST_WALK_STEPS;
            if self.inert(subject, 0, &mut b)? {
                let mut all_inert = true;
                let mut irest = rest;
                for k in 1..kids.len() {
                    let (arg, r) = self.kid(trait_ref, &kids, irest, k)?;
                    irest = r;
                    let arg = match self.resolve(arg)? {
                        Resolved::Side(s) => self.try_normalize_subject(s, 0),
                        Resolved::Unbound { .. } => {
                            all_inert = false;
                            break;
                        }
                    };
                    let mut kb = MOST_WALK_STEPS;
                    if !self.inert(arg, 0, &mut kb)? {
                        all_inert = false;
                        break;
                    }
                }
                if all_inert {
                    return Ok(());
                }
            }
        }
        if self.try_prove_trait_non_impl(subject, trait_ref, true, trait_)? {
            return Ok(());
        }
        let param_miss = matches!(self.node(subject)?, PNode::Param(_));
        let key = Keyer { walk: &*self, at: AskAt::SubjectKids { subject, of: trait_ref, len: kids.len() }, budget: Cell::new(MOST_WALK_STEPS) };
        let head = match selected.select_keyed(Ask { trait_, name: None, written: trait_ref.ty }, &key) {
            Selection::Impl { head, .. } => head,
            Selection::Not(NotSelected::Builtin) if self.nothing_to_tie(trait_ref, 1)? => return Ok(()),
            Selection::Not(NotSelected::Unanswered | NotSelected::Generic | NotSelected::GenericSelf(_)) if param_miss => {
                return Err(Refused::Unselected { item: self.item, node: trait_ref.ty, trait_, name: None, why: NotSelected::NoBound });
            }
            Selection::Not(why) => return Err(Refused::Unselected { item: self.item, node: trait_ref.ty, trait_, name: None, why }),
        };
        let env = self.envs.impl_env(head.lifetimes, head.types).ok_or(self.malformed(trait_ref.ty, Bad::StepBound))?;
        // The header in the clause's own form, subject-first ([`ImplHead::trait_ref`]): child
        // for child, the header's `SelfTy` against the subject.
        let header = Side { ty: head.trait_ref.ty, slots: Slots::Sig { col: head.trait_ref.col, env } };
        self.fits(header)?;
        let tl = self.own(header)?;
        let hkids = self.kids_of(header)?;
        if tl != own || kids.len() != hkids.len() {
            return Err(Refused::Shape { item: self.item, a: header.ty, b: trait_ref.ty, why: ShapeWhy::ImplHeader });
        }
        // The trait's lifetime arguments, invariantly.
        for k in 0..own.get() {
            let (x, y) = (self.slot(header, k)?, self.slot(trait_ref, k)?);
            self.pair(trait_ref.ty, x, y, Variance::In, Self::env_of(header), Self::env_of(trait_ref))?;
        }
        let (_, hrest) = header.slots.split(tl).ok_or(self.malformed(header.ty, Bad::StepBound))?;
        let (h0, mut hrest) = self.kid(header, &hkids, hrest, 0)?;
        self.walk(h0, subject, Variance::In, 1)?;
        for k in 1..hkids.len() {
            let (h, hr) = self.kid(header, &hkids, hrest, k)?;
            let (x, r) = self.kid(trait_ref, &kids, rest, k)?;
            hrest = hr;
            rest = r;
            self.walk(h, x, Variance::In, 1)?;
        }
        self.impl_where(head.imp, trait_, trait_ref.ty, env, head.lifetimes)
    }

    // ---- Non-impl proofs: builtin Fn*, owner bounds + supertraits, Dynamic, Opaque, Alias ----

    /// Builtin `Fn` / `FnMut` / `FnOnce` proof for a `FnPtr`, caller `Closure`, or `FnDef`.
    /// Relates `args_tuple` against the callable's inputs (and `want_output` against its output,
    /// when called from `proj_of`), returning the callable's output side on match.
    fn builtin_fn_trait(
        &mut self,
        subject: Side<'c>,
        args_tuple: Side<'c>,
        want_output: Option<Side<'c>>,
        _trait_: ItemKey,
        node: PTy,
    ) -> Result<Option<Side<'c>>, Refused> {
        let args_tuple = match self.resolve(args_tuple)? {
            Resolved::Side(s) => s,
            Resolved::Unbound { param, .. } => return Err(self.malformed(args_tuple.ty, Bad::Unconstrained { index: param })),
        };
        let PNode::Tuple(_) = self.node(args_tuple)? else { return Ok(None) };
        self.fits(args_tuple)?;
        let akids = self.kids_of(args_tuple)?;
        match self.node(subject)? {
            PNode::FnPtr(..) => self.relate_fn_ptr_sig(subject, args_tuple, &akids, want_output),
            PNode::Closure { owner, ordinal, .. } if owner == self.owner.item() => {
                let owner_ref: &'c dyn Owner = self.owner;
                let Some(csig) = owner_ref.closure_sig(ordinal) else { return Ok(None) };
                let cenv = self.envs.impl_env(csig.nsig, 0).ok_or(self.malformed(node, Bad::StepBound))?;
                for j in 0..csig.nsig {
                    match self.slot(subject, csig.ncap + j)? {
                        RegAt::Known(r) => self.envs.bind_reg(cenv, j, r).map_err(|w| self.malformed(node, w))?,
                        RegAt::Unbound { param, .. } => return Err(self.malformed(node, Bad::Unconstrained { index: param })),
                    }
                }
                let sig_side = Side { ty: csig.sig_ty, slots: Slots::CallerEnv { col: csig.col, env: cenv } };
                self.relate_fn_ptr_sig(sig_side, args_tuple, &akids, want_output)
            }
            PNode::FnDef(fn_item, _) => {
                let selected: &'c dyn Selected = self.selected;
                let Ok((params, sig_col, where_)) = selected.fn_def_sig(fn_item) else { return Ok(None) };
                self.fits(subject)?;
                let sub_own = self.own(subject)?;
                let sub_kids = self.kids_of(subject)?;
                let (_, mut srest) = subject.slots.split(sub_own).ok_or(self.malformed(subject.ty, Bad::StepBound))?;
                let fn_env = self.envs.impl_env(params.count(), sub_kids.len()).ok_or(self.malformed(node, Bad::StepBound))?;
                for k in 0..sub_kids.len() {
                    let (kid, r) = self.kid(subject, &sub_kids, srest, k)?;
                    srest = r;
                    self.envs.bind_type(fn_env, k, kid).map_err(|w| self.malformed(node, w))?;
                }
                for i in 0..params.count() {
                    let o = self.out.fresh(Kind::Existential);
                    self.envs.bind_reg(fn_env, i, Reg::Origin(o)).map_err(|w| self.malformed(node, w))?;
                }
                let sig_root = sig_col.ty();
                let sig_side = Side { ty: sig_root, slots: Slots::Sig { col: sig_col.as_slice(), env: fn_env } };
                let skids = self.kids_of(sig_side)?;
                let Some(n_in) = skids.len().checked_sub(1) else { return Ok(None) };
                if n_in != akids.len() {
                    return Ok(None);
                }
                let mut arest = args_tuple.slots;
                let mut fn_rest = sig_side.slots;
                for k in 0..n_in {
                    let (arg, ar) = self.kid(args_tuple, &akids, arest, k)?;
                    let (fin, fr) = self.kid(sig_side, &skids, fn_rest, k)?;
                    arest = ar;
                    fn_rest = fr;
                    self.walk_with_binder(arg, fin, Variance::Co, BinderCtx::FnPtr(Variance::Contra), 1)?;
                }
                let (fn_out, _) = self.kid(sig_side, &skids, fn_rest, n_in)?;
                // Enforce `fn_item`'s own region/type outlives bounds.
                for &(long, short) in params.outlives() {
                    let (l, s) = (self.known_in(fn_env, long, node)?, self.known_in(fn_env, short, node)?);
                    self.outlives(node, l, s)?;
                }
                for o in where_.outlives {
                    match *o {
                        Outlives::Region { long, short } => {
                            if !params.outlives().contains(&(long, short)) {
                                let (l, s) = (self.known_in(fn_env, long, node)?, self.known_in(fn_env, short, node)?);
                                self.outlives(node, l, s)?;
                            }
                        }
                        Outlives::Type { clause, region } => {
                            if let Some(cv) = where_.views.iter().find(|c| c.nth == clause) {
                                let r = self.known_in(fn_env, region, node)?;
                                self.components(Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env: fn_env } }, r, 0)?;
                            }
                        }
                    }
                }
                if let Some(term) = want_output {
                    self.walk_with_binder(fn_out, term, Variance::Co, BinderCtx::FnPtr(Variance::Co), 1)?;
                }
                Ok(Some(fn_out))
            }
            _ => Ok(None),
        }
    }

    fn relate_fn_ptr_sig(
        &mut self,
        fn_ptr: Side<'c>,
        args_tuple: Side<'c>,
        akids: &Kids<'c>,
        want_output: Option<Side<'c>>,
    ) -> Result<Option<Side<'c>>, Refused> {
        self.fits(fn_ptr)?;
        let fkids = self.kids_of(fn_ptr)?;
        let Some(n_in) = fkids.len().checked_sub(1) else { return Ok(None) };
        if n_in != akids.len() {
            return Ok(None);
        }
        let mut arest = args_tuple.slots;
        let mut frest = fn_ptr.slots;
        for k in 0..n_in {
            let (arg, ar) = self.kid(args_tuple, akids, arest, k)?;
            let (fin, fr) = self.kid(fn_ptr, &fkids, frest, k)?;
            arest = ar;
            frest = fr;
            self.walk_with_binder(arg, fin, Variance::Co, BinderCtx::FnPtr(Variance::Contra), 1)?;
        }
        let (fn_out, _) = self.kid(fn_ptr, &fkids, frest, n_in)?;
        if let Some(term) = want_output {
            self.walk_with_binder(fn_out, term, Variance::Co, BinderCtx::FnPtr(Variance::Co), 1)?;
        }
        Ok(Some(fn_out))
    }

    /// Match a subject-first `ExTrait(ct, [c_self, c_args..])` clause against `subject: trait_ref`
    /// (`has_subject`: whether `trait_ref` includes `subject` as child 0).
    fn match_trait_clause(&mut self, clause: Side<'c>, subject: Side<'c>, trait_ref: Side<'c>, has_subject: bool, allow_alias: bool) -> Result<bool, Refused> {
        let (Ok(PNode::ExTrait(ct, _)), Ok(PNode::ExTrait(wt, _))) = (self.node(clause), self.node(trait_ref)) else {
            return Ok(false);
        };
        if ct != wt {
            return Ok(false);
        }
        if self.fits(clause).is_err() {
            return Ok(false);
        }
        let (c_own, w_own) = (self.own(clause)?, self.own(trait_ref)?);
        let (ckids, wkids) = (self.kids_of(clause)?, self.kids_of(trait_ref)?);
        let non_self = if has_subject { wkids.len().saturating_sub(1) } else { wkids.len() };
        if c_own != w_own || ckids.len() != non_self + 1 {
            return Ok(false);
        }
        let Some(c_self) = self.erased_kid(clause, 0) else { return Ok(false) };
        if !self.erased_sides(c_self, subject) {
            return Ok(false);
        }
        let w_off = u32::from(has_subject);
        for k in 0..non_self {
            let (Some(ca), Some(wa)) = (self.erased_kid(clause, k + 1), self.erased_kid(trait_ref, k + w_off)) else {
                return Ok(false);
            };
            if !self.erased_sides_with(ca, wa, allow_alias) {
                return Ok(false);
            }
        }
        let saved_bv = self.binder_v;
        self.binder_v = BinderCtx::Clause;
        for k in 0..w_own.get() {
            let (x, y) = (self.slot(clause, k)?, self.slot(trait_ref, k)?);
            if let Err(e) = self.pair(trait_ref.ty, x, y, Variance::In, Self::env_of(clause), Self::env_of(trait_ref)) {
                self.binder_v = saved_bv;
                return Err(e);
            }
        }
        self.binder_v = saved_bv;
        let (_, crest) = clause.slots.split(c_own).ok_or(self.malformed(clause.ty, Bad::StepBound))?;
        let (c0, mut crest) = self.kid(clause, &ckids, crest, 0)?;
        self.walk_with_binder(c0, subject, Variance::In, BinderCtx::Clause, 1)?;
        let (_, mut wrest) = trait_ref.slots.split(w_own).ok_or(self.malformed(trait_ref.ty, Bad::StepBound))?;
        if has_subject {
            let (_, wr) = self.kid(trait_ref, &wkids, wrest, 0)?;
            wrest = wr;
        }
        for k in 0..non_self {
            let (c, cr) = self.kid(clause, &ckids, crest, k + 1)?;
            let (w, wr) = self.kid(trait_ref, &wkids, wrest, k + w_off)?;
            crest = cr;
            wrest = wr;
            self.walk_with_binder(c, w, Variance::In, BinderCtx::Clause, 1)?;
        }
        Ok(true)
    }

    /// Match a subject-less `ExTrait(ct, [c_args..])` (from `PNode::Dynamic`) against `trait_ref`.
    fn match_trait_no_self(&mut self, pred: Side<'c>, trait_ref: Side<'c>, has_subject: bool, allow_alias: bool) -> Result<bool, Refused> {
        let (Ok(PNode::ExTrait(ct, _)), Ok(PNode::ExTrait(wt, _))) = (self.node(pred), self.node(trait_ref)) else {
            return Ok(false);
        };
        if ct != wt || self.fits(pred).is_err() {
            return Ok(false);
        }
        let (p_own, w_own) = (self.own(pred)?, self.own(trait_ref)?);
        let (pkids, wkids) = (self.kids_of(pred)?, self.kids_of(trait_ref)?);
        let non_self = if has_subject { wkids.len().saturating_sub(1) } else { wkids.len() };
        if p_own != w_own || pkids.len() != non_self {
            return Ok(false);
        }
        let w_off = u32::from(has_subject);
        for k in 0..non_self {
            let (Some(pa), Some(wa)) = (self.erased_kid(pred, k), self.erased_kid(trait_ref, k + w_off)) else {
                return Ok(false);
            };
            if !self.erased_sides_with(pa, wa, allow_alias) {
                return Ok(false);
            }
        }
        for k in 0..w_own.get() {
            let (x, y) = (self.slot(pred, k)?, self.slot(trait_ref, k)?);
            self.pair(trait_ref.ty, x, y, Variance::In, Self::env_of(pred), Self::env_of(trait_ref))?;
        }
        let (_, mut prest) = pred.slots.split(p_own).ok_or(self.malformed(pred.ty, Bad::StepBound))?;
        let (_, mut wrest) = trait_ref.slots.split(w_own).ok_or(self.malformed(trait_ref.ty, Bad::StepBound))?;
        if has_subject {
            let (_, wr) = self.kid(trait_ref, &wkids, wrest, 0)?;
            wrest = wr;
        }
        for k in 0..non_self {
            let (p, pr) = self.kid(pred, &pkids, prest, k)?;
            let (w, wr) = self.kid(trait_ref, &wkids, wrest, k + w_off)?;
            prest = pr;
            wrest = wr;
            self.walk_with_binder(p, w, Variance::In, BinderCtx::Clause, 1)?;
        }
        Ok(true)
    }

    /// Match a subject-first `ExProj { trait_, name, [c_self, c_args..], term }` clause against
    /// `want` (an `Alias` or `ExProj` with `want_arg_count` arguments `[Self, A..]`), returning
    /// the clause's `term` side on match.
    fn match_proj_clause(
        &mut self,
        clause: Side<'c>,
        want: Side<'c>,
        want_arg_count: u32,
        want_trait: ItemKey,
        want_name: blaze_typeck::Name,
        allow_alias: bool,
    ) -> Result<Option<Side<'c>>, Refused> {
        let Ok(PNode::ExProj { trait_: ct, name: cn, .. }) = self.node(clause) else { return Ok(None) };
        if ct != want_trait || cn != want_name || self.fits(clause).is_err() {
            return Ok(None);
        }
        let (c_own, w_own) = (self.own(clause)?, self.own(want)?);
        let (ckids, wkids) = (self.kids_of(clause)?, self.kids_of(want)?);
        if c_own != w_own || ckids.len() != want_arg_count + 1 {
            return Ok(None);
        }
        for k in 0..want_arg_count {
            let (Some(ca), Some(wa)) = (self.erased_kid(clause, k), self.erased_kid(want, k)) else {
                return Ok(None);
            };
            let ok = if k == 0 { self.erased_sides(ca, wa) } else { self.erased_sides_with(ca, wa, allow_alias) };
            if !ok {
                return Ok(None);
            }
        }
        let saved_bv = self.binder_v;
        self.binder_v = BinderCtx::Clause;
        for k in 0..w_own.get() {
            let (x, y) = (self.slot(clause, k)?, self.slot(want, k)?);
            if let Err(e) = self.pair(want.ty, x, y, Variance::In, Self::env_of(clause), Self::env_of(want)) {
                self.binder_v = saved_bv;
                return Err(e);
            }
        }
        self.binder_v = saved_bv;
        let (_, mut crest) = clause.slots.split(c_own).ok_or(self.malformed(clause.ty, Bad::StepBound))?;
        let (_, mut wrest) = want.slots.split(w_own).ok_or(self.malformed(want.ty, Bad::StepBound))?;
        for k in 0..want_arg_count {
            let (c, cr) = self.kid(clause, &ckids, crest, k)?;
            let (w, wr) = self.kid(want, &wkids, wrest, k)?;
            crest = cr;
            wrest = wr;
            self.walk_with_binder(c, w, Variance::In, BinderCtx::Clause, 1)?;
        }
        let (term, _) = self.kid(clause, &ckids, crest, want_arg_count)?;
        Ok(Some(term))
    }

    /// Build a trait's environment from a proven `seed_trait` (`ExTrait(seed_tr, ..)`), binding
    /// its lifetime params from `seed_trait`'s own slots and `Param(0..)` from `[seed_self, args..]`.
    fn trait_env_from_seed(&mut self, seed_tr: ItemKey, seed_self: Side<'c>, seed_trait: Side<'c>, seed_has_self: bool) -> Result<Option<(EnvId, &'c [blaze_typeck::region_facts::ClauseView<'c>])>, Refused> {
        let selected: &'c dyn Selected = self.selected;
        let Some((tr_lts, views)) = selected.trait_clauses(seed_tr) else { return Ok(None) };
        if views.is_empty() || self.fits(seed_trait).is_err() {
            return Ok(None);
        }
        let own = self.own(seed_trait)?;
        let kids = self.kids_of(seed_trait)?;
        let n_types = if seed_has_self { kids.len() } else { kids.len() + 1 };
        let lts = tr_lts.max(own.get());
        let Some(tenv) = self.envs.impl_env(lts, n_types) else { return Ok(None) };
        for k in 0..own.get() {
            match self.slot(seed_trait, k)? {
                RegAt::Known(r) => {
                    if self.envs.bind_reg(tenv, k, r).is_err() {
                        return Ok(None);
                    }
                }
                RegAt::Unbound { .. } => return Ok(None),
            }
        }
        for k in own.get()..lts {
            let o = self.out.fresh(Kind::Existential);
            let _ = self.envs.bind_reg(tenv, k, Reg::Origin(o));
        }
        let (_, mut rest) = seed_trait.slots.split(own).ok_or(self.malformed(seed_trait.ty, Bad::StepBound))?;
        if seed_has_self {
            for k in 0..kids.len() {
                let (c, r) = self.kid(seed_trait, &kids, rest, k)?;
                rest = r;
                let _ = self.envs.bind_type(tenv, k, c);
            }
        } else {
            let _ = self.envs.bind_type(tenv, 0, seed_self);
            for k in 0..kids.len() {
                let (c, r) = self.kid(seed_trait, &kids, rest, k)?;
                rest = r;
                let _ = self.envs.bind_type(tenv, k + 1, c);
            }
        }
        Ok(Some((tenv, views)))
    }

    #[allow(clippy::too_many_arguments)]
    fn elaborate_for_trait(
        &mut self,
        seed_tr: ItemKey,
        seed_self: Side<'c>,
        seed_trait: Side<'c>,
        seed_has_self: bool,
        want_tr: ItemKey,
        subject: Side<'c>,
        trait_ref: Side<'c>,
        has_subject: bool,
        depth: u32,
        visited: &mut [ItemKey; 16],
        vlen: &mut usize,
    ) -> Result<bool, Refused> {
        if depth >= 8 || *vlen >= 16 || visited[..*vlen].contains(&seed_tr) {
            return Ok(false);
        }
        visited[*vlen] = seed_tr;
        *vlen += 1;
        let Some((tenv, views)) = self.trait_env_from_seed(seed_tr, seed_self, seed_trait, seed_has_self)? else {
            return Ok(false);
        };
        for allow_alias in [false, true] {
            for cv in views {
                let c = Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env: tenv } };
                if self.match_trait_clause(c, subject, trait_ref, has_subject, allow_alias)? {
                    return Ok(true);
                }
            }
        }
        for cv in views {
            let c = Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env: tenv } };
            if let Ok(PNode::ExTrait(ct, _)) = self.node(c)
                && self.fits(c).is_ok()
                && let Ok(c_own) = self.own(c)
                && let Ok(ckids) = self.kids_of(c)
                && ckids.len() > 0
                && let Some((_, crest)) = c.slots.split(c_own)
                && let Ok((c_self, _)) = self.kid(c, &ckids, crest, 0)
                && self.erased_sides(c_self, subject)
                && self.elaborate_for_trait(ct, c_self, c, true, want_tr, subject, trait_ref, has_subject, depth + 1, visited, vlen)?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    #[allow(clippy::too_many_arguments)]
    fn elaborate_for_proj(
        &mut self,
        seed_tr: ItemKey,
        seed_self: Side<'c>,
        seed_trait: Side<'c>,
        seed_has_self: bool,
        want: Side<'c>,
        want_arg_count: u32,
        subject: Side<'c>,
        want_tr: ItemKey,
        want_name: blaze_typeck::Name,
        depth: u32,
        visited: &mut [ItemKey; 16],
        vlen: &mut usize,
    ) -> Result<Option<Side<'c>>, Refused> {
        if depth >= 8 || *vlen >= 16 || visited[..*vlen].contains(&seed_tr) {
            return Ok(None);
        }
        visited[*vlen] = seed_tr;
        *vlen += 1;
        let Some((tenv, views)) = self.trait_env_from_seed(seed_tr, seed_self, seed_trait, seed_has_self)? else {
            return Ok(None);
        };
        for allow_alias in [false, true] {
            for cv in views {
                let c = Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env: tenv } };
                if let Some(term) = self.match_proj_clause(c, want, want_arg_count, want_tr, want_name, allow_alias)? {
                    return Ok(Some(term));
                }
            }
        }
        for cv in views {
            let c = Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env: tenv } };
            if let Ok(PNode::ExTrait(ct, _)) = self.node(c)
                && self.fits(c).is_ok()
                && let Ok(c_own) = self.own(c)
                && let Ok(ckids) = self.kids_of(c)
                && ckids.len() > 0
                && let Some((_, crest)) = c.slots.split(c_own)
                && let Ok((c_self, _)) = self.kid(c, &ckids, crest, 0)
                && self.erased_sides(c_self, subject)
                && let Some(term) = self.elaborate_for_proj(ct, c_self, c, true, want, want_arg_count, subject, want_tr, want_name, depth + 1, visited, vlen)?
            {
                return Ok(Some(term));
            }
        }
        Ok(None)
    }

    /// Bind an opaque type's defining-item env from a clause whose child 0 is `Opaque(op_key, ..)`.
    fn opaque_clause_side(&mut self, cv: &blaze_typeck::region_facts::ClauseView<'c>, op_lts: u32, subject: Side<'c>, op_key: ItemKey) -> Result<Option<Side<'c>>, Refused> {
        let sub_kids = self.kids_of(subject)?;
        let Some(oenv) = self.envs.impl_env(op_lts, sub_kids.len()) else { return Ok(None) };
        let c = Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env: oenv } };
        if self.fits(c).is_err() {
            return Ok(None);
        }
        let c_own = self.own(c)?;
        let ckids = self.kids_of(c)?;
        if ckids.len() == 0 {
            return Ok(None);
        }
        let Some((_, crest)) = c.slots.split(c_own) else { return Ok(None) };
        let (c0, _) = self.kid(c, &ckids, crest, 0)?;
        if !matches!(self.node(c0)?, PNode::Opaque(k, _) if k == op_key) {
            return Ok(None);
        }
        // Walking `c0` against `subject` binds every captured lifetime and type param of `oenv`.
        if self.walk(c0, subject, Variance::In, 1).is_err() {
            return Ok(None);
        }
        for i in 0..op_lts {
            if let Ok(RegAt::Unbound { param, .. }) = self.envs.reg(oenv, Region::Param(i)) {
                let o = self.out.fresh(Kind::Existential);
                let _ = self.envs.bind_reg(oenv, param, Reg::Origin(o));
            }
        }
        Ok(Some(c))
    }

    /// Try to prove `subject: trait_ref` from non-impl sources: builtin `Fn*`, caller bounds
    /// (including compound subjects and transitive supertraits), `dyn` object predicates,
    /// `impl Trait` opaque bounds, or associated type trait bounds.
    fn try_prove_trait_non_impl(&mut self, subject: Side<'c>, trait_ref: Side<'c>, has_subject: bool, trait_: ItemKey) -> Result<bool, Refused> {
        // 1. Builtin `Fn` / `FnMut` / `FnOnce` on `FnPtr`, caller `Closure`, or `FnDef`.
        if self.selected.is_fn_trait(trait_) {
            let own = self.own(trait_ref)?;
            let kids = self.kids_of(trait_ref)?;
            let arg_idx = u32::from(has_subject);
            if kids.len() == arg_idx + 1 {
                let (_, mut rest) = trait_ref.slots.split(own).ok_or(self.malformed(trait_ref.ty, Bad::StepBound))?;
                if has_subject {
                    let (_, r0) = self.kid(trait_ref, &kids, rest, 0)?;
                    rest = r0;
                }
                let (args_tuple, _) = self.kid(trait_ref, &kids, rest, arg_idx)?;
                if self.builtin_fn_trait(subject, args_tuple, None, trait_, trait_ref.ty)?.is_some() {
                    return Ok(true);
                }
            }
        }
        let owner: &'c dyn Owner = self.owner;
        let clauses = owner.clauses();
        // 2. Direct `Param(p)` bound via `Owner::bound` ONLY when `Owner::clauses` is empty
        //    (unit-test stubs). When `clauses` is populated, step 3 matches ALL trait arguments
        //    so multi-bound parameters like `Idx: PartialOrd<Idx> + PartialOrd<U>` pick the right one.
        if clauses.is_empty()
            && let PNode::Param(p) = self.node(subject)?
            && let Some(bound) = owner.bound(p, trait_)
        {
            let env = self.envs.owner_env().ok_or(self.malformed(trait_ref.ty, Bad::StepBound))?;
            let b = Side { ty: bound.ty, slots: Slots::Sig { col: bound.col, env } };
            self.walk(trait_ref, b, Variance::In, 1)?;
            return Ok(true);
        }
        // 3. Caller's `where`-clauses (any subject: `Param`, `Alias`, compound `OnceCell<T>`, etc.)
        //    plus transitive supertraits.
        if !clauses.is_empty() {
            let oenv = self.envs.owner_env().ok_or(self.malformed(trait_ref.ty, Bad::StepBound))?;
            for allow_alias in [false, true] {
                for cv in clauses {
                    let c = Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env: oenv } };
                    if self.match_trait_clause(c, subject, trait_ref, has_subject, allow_alias)? {
                        return Ok(true);
                    }
                }
            }
            let mut visited = [ItemKey(0); 16];
            let mut vlen = 0usize;
            for cv in clauses {
                let c = Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env: oenv } };
                if let Ok(PNode::ExTrait(ct, _)) = self.node(c)
                    && self.fits(c).is_ok()
                    && let Ok(c_own) = self.own(c)
                    && let Ok(ckids) = self.kids_of(c)
                    && ckids.len() > 0
                    && let Some((_, crest)) = c.slots.split(c_own)
                    && let Ok((c_self, _)) = self.kid(c, &ckids, crest, 0)
                    && self.erased_sides(c_self, subject)
                    && self.elaborate_for_trait(ct, c_self, c, true, trait_, subject, trait_ref, has_subject, 0, &mut visited, &mut vlen)?
                {
                    return Ok(true);
                }
            }
        }
        // 4. Subject-specific bounds: `Dynamic`, `Opaque`, `Alias`.
        match self.node(subject)? {
            PNode::Dynamic(_) => {
                self.fits(subject)?;
                let s_own = self.own(subject)?;
                let skids = self.kids_of(subject)?;
                for allow_alias in [false, true] {
                    let (_, mut srest) = subject.slots.split(s_own).ok_or(self.malformed(subject.ty, Bad::StepBound))?;
                    for k in 0..skids.len() {
                        let (pred, sr) = self.kid(subject, &skids, srest, k)?;
                        srest = sr;
                        match self.node(pred)? {
                            PNode::ExAuto(at) if at == trait_ => return Ok(true),
                            PNode::ExTrait(pt, _) if pt == trait_ => {
                                if self.match_trait_no_self(pred, trait_ref, has_subject, allow_alias)? {
                                    return Ok(true);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                let (_, mut srest) = subject.slots.split(s_own).ok_or(self.malformed(subject.ty, Bad::StepBound))?;
                let mut visited = [ItemKey(0); 16];
                let mut vlen = 0usize;
                for k in 0..skids.len() {
                    let (pred, sr) = self.kid(subject, &skids, srest, k)?;
                    srest = sr;
                    if let PNode::ExTrait(pt, _) = self.node(pred)?
                        && self.elaborate_for_trait(pt, subject, pred, false, trait_, subject, trait_ref, has_subject, 0, &mut visited, &mut vlen)?
                    {
                        return Ok(true);
                    }
                }
            }
            PNode::Opaque(op_key, _) => {
                let selected: &'c dyn Selected = self.selected;
                if let Some((op_lts, views)) = selected.opaque_clauses(op_key) {
                    for allow_alias in [false, true] {
                        for cv in views {
                            if let Ok(PNode::ExTrait(ct, _)) = self.sig_types.node(cv.col.ty())
                                && ct == trait_
                                && let Some(c) = self.opaque_clause_side(cv, op_lts, subject, op_key)?
                                && self.match_trait_clause(c, subject, trait_ref, has_subject, allow_alias)?
                            {
                                return Ok(true);
                            }
                        }
                    }
                    let mut visited = [ItemKey(0); 16];
                    let mut vlen = 0usize;
                    for cv in views {
                        if let Ok(PNode::ExTrait(ct, _)) = self.sig_types.node(cv.col.ty())
                            && let Some(c) = self.opaque_clause_side(cv, op_lts, subject, op_key)?
                            && self.elaborate_for_trait(ct, subject, c, true, trait_, subject, trait_ref, has_subject, 0, &mut visited, &mut vlen)?
                        {
                            return Ok(true);
                        }
                    }
                }
            }
            PNode::Alias { trait_: at, name, .. } => {
                let selected: &'c dyn Selected = self.selected;
                // Check the associated type's own item bounds (`type Searcher<'a>: Searcher<'a>`,
                // `type Structural: UniqueSet`) first, then the trait's own clauses.
                for source in [selected.assoc_clauses(at, name), selected.trait_clauses(at)] {
                    if let Some((at_lts, views)) = source
                        && !views.is_empty()
                        && self.fits(subject).is_ok()
                        && self.try_prove_from_alias_clauses(at_lts, views, subject, trait_ref, has_subject, trait_)?
                    {
                        return Ok(true);
                    }
                }
            }
            _ => {}
        }
        Ok(false)
    }

    fn try_prove_from_alias_clauses(
        &mut self,
        at_lts: u32,
        views: &'c [blaze_typeck::region_facts::ClauseView<'c>],
        subject: Side<'c>,
        trait_ref: Side<'c>,
        has_subject: bool,
        trait_: ItemKey,
    ) -> Result<bool, Refused> {
        let s_own = self.own(subject)?;
        let skids = self.kids_of(subject)?;
        let lts = at_lts.max(s_own.get());
        let Some(aenv) = self.envs.impl_env(lts, skids.len()) else { return Ok(false) };
        for k in 0..at_lts.min(s_own.get()) {
            match self.slot(subject, k)? {
                RegAt::Known(r) => {
                    if self.envs.bind_reg(aenv, k, r).is_err() {
                        return Ok(false);
                    }
                }
                RegAt::Unbound { .. } => return Ok(false),
            }
        }
        for k in at_lts.min(s_own.get())..lts {
            let o = self.out.fresh(Kind::Existential);
            let _ = self.envs.bind_reg(aenv, k, Reg::Origin(o));
        }
        let (_, mut srest) = subject.slots.split(s_own).ok_or(self.malformed(subject.ty, Bad::StepBound))?;
        for k in 0..skids.len() {
            let (kid, sr) = self.kid(subject, &skids, srest, k)?;
            srest = sr;
            let _ = self.envs.bind_type(aenv, k, kid);
        }
        for allow_alias in [false, true] {
            for cv in views {
                let c = Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env: aenv } };
                if self.match_trait_clause(c, subject, trait_ref, has_subject, allow_alias)? {
                    return Ok(true);
                }
            }
        }
        let mut visited = [ItemKey(0); 16];
        let mut vlen = 0usize;
        for cv in views {
            let c = Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env: aenv } };
            if let Ok(PNode::ExTrait(ct, _)) = self.node(c)
                && self.fits(c).is_ok()
                && let Ok(c_own) = self.own(c)
                && let Ok(ckids) = self.kids_of(c)
                && ckids.len() > 0
                && let Some((_, crest)) = c.slots.split(c_own)
                && let Ok((c_self, _)) = self.kid(c, &ckids, crest, 0)
                && self.erased_sides(c_self, subject)
                && self.elaborate_for_trait(ct, c_self, c, true, trait_, subject, trait_ref, has_subject, 0, &mut visited, &mut vlen)?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Try to normalize an `Alias` or `ExProj` (with `want_arg_count` arguments `[Self, A..]`)
    /// from non-impl sources: builtin `FnOnce::Output`, caller `ExProj` bounds + supertraits,
    /// `Dynamic` `ExProj` predicates, `Opaque` bounds, or `Alias` trait bounds.
    fn try_normalize_non_impl(
        &mut self,
        want: Side<'c>,
        want_arg_count: u32,
        subject: Side<'c>,
        trait_: ItemKey,
        name: blaze_typeck::Name,
    ) -> Result<Option<Side<'c>>, Refused> {
        // 1. Builtin `FnOnce::Output` (when called from `normalize`; `proj_of` handles `want_output` directly).
        if self.selected.is_fn_trait(trait_) && want_arg_count == 2 {
            let own = self.own(want)?;
            let kids = self.kids_of(want)?;
            let (_, rest) = want.slots.split(own).ok_or(self.malformed(want.ty, Bad::StepBound))?;
            let (_, rest) = self.kid(want, &kids, rest, 0)?;
            let (args_tuple, _) = self.kid(want, &kids, rest, 1)?;
            if let Some(out) = self.builtin_fn_trait(subject, args_tuple, None, trait_, want.ty)? {
                return Ok(Some(out));
            }
        }
        let owner: &'c dyn Owner = self.owner;
        let clauses = owner.clauses();
        // 2. Direct `Param(p)` projection bound via `Owner::proj_bound` (unit-test stubs).
        if clauses.is_empty()
            && let PNode::Param(p) = self.node(subject)?
            && let Some(bound) = owner.proj_bound(p, trait_, name)
        {
            let env = self.envs.owner_env().ok_or(self.malformed(want.ty, Bad::StepBound))?;
            let b = Side { ty: bound.ty, slots: Slots::Sig { col: bound.col, env } };
            if let Some(term) = self.match_proj_clause(b, want, want_arg_count, trait_, name, false)? {
                return Ok(Some(term));
            }
        }
        // 3. Caller's `where`-clauses + transitive supertraits.
        if !clauses.is_empty() {
            let oenv = self.envs.owner_env().ok_or(self.malformed(want.ty, Bad::StepBound))?;
            for allow_alias in [false, true] {
                for cv in clauses {
                    let c = Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env: oenv } };
                    if let Some(term) = self.match_proj_clause(c, want, want_arg_count, trait_, name, allow_alias)? {
                        return Ok(Some(term));
                    }
                }
            }
            let mut visited = [ItemKey(0); 16];
            let mut vlen = 0usize;
            for cv in clauses {
                let c = Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env: oenv } };
                if let Ok(PNode::ExTrait(ct, _)) = self.node(c)
                    && self.fits(c).is_ok()
                    && let Ok(c_own) = self.own(c)
                    && let Ok(ckids) = self.kids_of(c)
                    && ckids.len() > 0
                    && let Some((_, crest)) = c.slots.split(c_own)
                    && let Ok((c_self, _)) = self.kid(c, &ckids, crest, 0)
                    && self.erased_sides(c_self, subject)
                    && let Some(term) = self.elaborate_for_proj(ct, c_self, c, true, want, want_arg_count, subject, trait_, name, 0, &mut visited, &mut vlen)?
                {
                    return Ok(Some(term));
                }
            }
        }
        // 4. Subject-specific bounds: `Dynamic`, `Opaque`, `Alias`.
        match self.node(subject)? {
            PNode::Dynamic(_) => {
                self.fits(subject)?;
                let s_own = self.own(subject)?;
                let skids = self.kids_of(subject)?;
                let w_own = self.own(want)?;
                let wkids = self.kids_of(want)?;
                let non_self = want_arg_count.saturating_sub(1);
                for allow_alias in [false, true] {
                    let (_, mut srest) = subject.slots.split(s_own).ok_or(self.malformed(subject.ty, Bad::StepBound))?;
                    for k in 0..skids.len() {
                        let (pred, sr) = self.kid(subject, &skids, srest, k)?;
                        srest = sr;
                        if let PNode::ExProj { trait_: pt, name: pn, .. } = self.node(pred)?
                            && pt == trait_
                            && pn == name
                            && self.fits(pred).is_ok()
                        {
                            let p_own = self.own(pred)?;
                            let pkids = self.kids_of(pred)?;
                            if p_own == w_own && pkids.len() == non_self + 1 {
                                let mut same = true;
                                for j in 0..non_self {
                                    let (Some(pa), Some(wa)) = (self.erased_kid(pred, j), self.erased_kid(want, j + 1)) else {
                                        same = false;
                                        break;
                                    };
                                    if !self.erased_sides_with(pa, wa, allow_alias) {
                                        same = false;
                                        break;
                                    }
                                }
                                if same {
                                    for j in 0..w_own.get() {
                                        let (x, y) = (self.slot(pred, j)?, self.slot(want, j)?);
                                        self.pair(want.ty, x, y, Variance::In, Self::env_of(pred), Self::env_of(want))?;
                                    }
                                    let (_, mut prest) = pred.slots.split(p_own).ok_or(self.malformed(pred.ty, Bad::StepBound))?;
                                    let (_, wrest0) = want.slots.split(w_own).ok_or(self.malformed(want.ty, Bad::StepBound))?;
                                    let (_, mut wrest) = self.kid(want, &wkids, wrest0, 0)?;
                                    for j in 0..non_self {
                                        let (p, pr) = self.kid(pred, &pkids, prest, j)?;
                                        let (w, wr) = self.kid(want, &wkids, wrest, j + 1)?;
                                        prest = pr;
                                        wrest = wr;
                                        self.walk_with_binder(p, w, Variance::In, BinderCtx::Clause, 1)?;
                                    }
                                    let (term, _) = self.kid(pred, &pkids, prest, non_self)?;
                                    return Ok(Some(term));
                                }
                            }
                        }
                    }
                }
                let (_, mut srest) = subject.slots.split(s_own).ok_or(self.malformed(subject.ty, Bad::StepBound))?;
                let mut visited = [ItemKey(0); 16];
                let mut vlen = 0usize;
                for k in 0..skids.len() {
                    let (pred, sr) = self.kid(subject, &skids, srest, k)?;
                    srest = sr;
                    if let PNode::ExTrait(pt, _) = self.node(pred)?
                        && let Some(term) = self.elaborate_for_proj(pt, subject, pred, false, want, want_arg_count, subject, trait_, name, 0, &mut visited, &mut vlen)?
                    {
                        return Ok(Some(term));
                    }
                }
            }
            PNode::Opaque(op_key, _) => {
                let selected: &'c dyn Selected = self.selected;
                if let Some((op_lts, views)) = selected.opaque_clauses(op_key) {
                    for allow_alias in [false, true] {
                        for cv in views {
                            if let Ok(PNode::ExProj { trait_: ct, name: cn, .. }) = self.sig_types.node(cv.col.ty())
                                && ct == trait_
                                && cn == name
                                && let Some(c) = self.opaque_clause_side(cv, op_lts, subject, op_key)?
                                && let Some(term) = self.match_proj_clause(c, want, want_arg_count, trait_, name, allow_alias)?
                            {
                                return Ok(Some(term));
                            }
                        }
                    }
                    let mut visited = [ItemKey(0); 16];
                    let mut vlen = 0usize;
                    for cv in views {
                        if let Ok(PNode::ExTrait(ct, _)) = self.sig_types.node(cv.col.ty())
                            && let Some(c) = self.opaque_clause_side(cv, op_lts, subject, op_key)?
                            && let Some(term) = self.elaborate_for_proj(ct, subject, c, true, want, want_arg_count, subject, trait_, name, 0, &mut visited, &mut vlen)?
                        {
                            return Ok(Some(term));
                        }
                    }
                }
            }
            PNode::Alias { trait_: at, name: an, .. } => {
                let selected: &'c dyn Selected = self.selected;
                for source in [selected.assoc_clauses(at, an), selected.trait_clauses(at)] {
                    if let Some((at_lts, views)) = source
                        && !views.is_empty()
                        && self.fits(subject).is_ok()
                    {
                        let s_own = self.own(subject)?;
                        let skids = self.kids_of(subject)?;
                        let lts = at_lts.max(s_own.get());
                        let Some(aenv) = self.envs.impl_env(lts, skids.len()) else { continue };
                        let mut ok = true;
                        for k in 0..at_lts.min(s_own.get()) {
                            match self.slot(subject, k)? {
                                RegAt::Known(r) => {
                                    if self.envs.bind_reg(aenv, k, r).is_err() {
                                        ok = false;
                                        break;
                                    }
                                }
                                RegAt::Unbound { .. } => {
                                    ok = false;
                                    break;
                                }
                            }
                        }
                        if !ok {
                            continue;
                        }
                        for k in at_lts.min(s_own.get())..lts {
                            let o = self.out.fresh(Kind::Existential);
                            let _ = self.envs.bind_reg(aenv, k, Reg::Origin(o));
                        }
                        let (_, mut srest) = subject.slots.split(s_own).ok_or(self.malformed(subject.ty, Bad::StepBound))?;
                        for k in 0..skids.len() {
                            let (kid, sr) = self.kid(subject, &skids, srest, k)?;
                            srest = sr;
                            let _ = self.envs.bind_type(aenv, k, kid);
                        }
                        for allow_alias in [false, true] {
                            for cv in views {
                                let c = Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env: aenv } };
                                if let Some(term) = self.match_proj_clause(c, want, want_arg_count, trait_, name, allow_alias)? {
                                    return Ok(Some(term));
                                }
                            }
                        }
                        let mut visited = [ItemKey(0); 16];
                        let mut vlen = 0usize;
                        for cv in views {
                            let c = Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env: aenv } };
                            if let Ok(PNode::ExTrait(ct, _)) = self.node(c)
                                && self.fits(c).is_ok()
                                && let Ok(c_own) = self.own(c)
                                && let Ok(ckids) = self.kids_of(c)
                                && ckids.len() > 0
                                && let Some((_, crest)) = c.slots.split(c_own)
                                && let Ok((c_self, _)) = self.kid(c, &ckids, crest, 0)
                                && self.erased_sides(c_self, subject)
                                && let Some(term) = self.elaborate_for_proj(ct, c_self, c, true, want, want_arg_count, subject, trait_, name, 0, &mut visited, &mut vlen)?
                            {
                                return Ok(Some(term));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(None)
    }
}

/// Two nodes of one head: one variant and the same non-type payload. Children are walked.
/// The ONE home of the rule (rule 30): the flow's own signature laying (blaze-flow-lower
/// grain.rs `lay_slots`) walks a declared type beside the body's by it too.
///
/// A body type (a closure, an async block's future, another body's coroutine) is one head per
/// expression: the same closure of the same owner, the same future expression. Its slots are
/// all its own (no children: blaze-flow-lower grain.rs `BodyTypes`), related invariantly by
/// [`crate::types::rule_variance`] (rustc's `relate_args_invariantly` for closures and
/// coroutines).
pub fn head_eq(a: PNode, b: PNode) -> bool {
    match (a, b) {
        (PNode::Closure { owner: o, ordinal: x, .. }, PNode::Closure { owner: p, ordinal: y, .. })
        | (PNode::Coroutine { owner: o, ordinal: x, .. }, PNode::Coroutine { owner: p, ordinal: y, .. }) => o == p && x == y,
        (PNode::Future(x, ..), PNode::Future(y, ..)) => x == y,
        (PNode::Bool, PNode::Bool) | (PNode::Char, PNode::Char) | (PNode::Str, PNode::Str) | (PNode::Never, PNode::Never) => true,
        (PNode::Error, PNode::Error) => true,
        (PNode::Int(x), PNode::Int(y)) => x == y,
        (PNode::Uint(x), PNode::Uint(y)) => x == y,
        (PNode::Float(x), PNode::Float(y)) => x == y,
        (PNode::Tuple(_), PNode::Tuple(_)) | (PNode::Union(_), PNode::Union(_)) | (PNode::Slice(_), PNode::Slice(_)) | (PNode::Dynamic(_), PNode::Dynamic(_)) => true,
        (PNode::Record(s1, _), PNode::Record(s2, _)) => s1 == s2,
        (PNode::Ref(m, _), PNode::Ref(n, _)) | (PNode::Ptr(m, _), PNode::Ptr(n, _)) => m == n,
        // The length is a const: equal by the type checker.
        (PNode::Array(..), PNode::Array(..)) | (PNode::Pat(..), PNode::Pat(..)) => true,
        (PNode::Adt(x, _), PNode::Adt(y, _))
        | (PNode::FnDef(x, _), PNode::FnDef(y, _))
        | (PNode::Opaque(x, _), PNode::Opaque(y, _))
        | (PNode::ExTrait(x, _), PNode::ExTrait(y, _))
        | (PNode::ExAuto(x), PNode::ExAuto(y))
        | (PNode::Foreign(x), PNode::Foreign(y)) => x == y,
        (PNode::FnPtr(_, h), PNode::FnPtr(_, g)) => h == g,
        (PNode::Alias { trait_: t, name: n, .. }, PNode::Alias { trait_: u, name: m, .. })
        | (PNode::ExProj { trait_: t, name: n, .. }, PNode::ExProj { trait_: u, name: m, .. }) => t == u && n == m,
        (PNode::Param(x), PNode::Param(y)) | (PNode::ConstParam(x), PNode::ConstParam(y)) => x == y,
        (PNode::ConstValue { .. }, PNode::ConstValue { .. }) => a == b,
        _ => false,
    }
}

/// A const generic argument's node: a value or a const parameter. It holds no region.
fn is_const(n: PNode) -> bool {
    matches!(n, PNode::ConstValue { .. } | PNode::ConstParam(_))
}

fn walker<'w, 'c, 'k: 'c, A: Allocator + Copy>(cx: &'w mut Cx<'k, '_>, envs: &'w mut Envs<'c, A>, item: ItemKey) -> Walk<'w, 'c, A> {
    Walk {
        types: cx.types,
        sig_types: cx.sig_types,
        selected: cx.selected,
        owner: cx.owner,
        origins: cx.origins,
        out: &mut *cx.out,
        envs,
        item,
        budget: MOST_WALK_STEPS,
        bound: [None; MOST_BOUND_PAIRS],
        bound_len: 0,
        hr: [None; MOST_BOUND_PAIRS],
        hr_len: 0,
        binder_v: BinderCtx::None,
        nested: 0,
        walk_stack: [None; WALK_STACK_CAP],
        walk_sp: 0,
        comp_stack: [None; COMP_STACK_CAP],
        comp_sp: 0,
    }
}

/// `a` flows where `b` is wanted, under `v`, over the call's environments `envs`; `item` the
/// item a refusal names.
pub fn relate_sides<'c, 'k: 'c, A: Allocator + Copy>(
    cx: &mut Cx<'k, '_>,
    envs: &mut Envs<'c, A>,
    item: ItemKey,
    a: Side<'c>,
    b: Side<'c>,
    v: Variance,
) -> Result<(), Refused> {
    walker(cx, envs, item).walk(a, b, v, 0)
}

/// Two of the caller's values related under `v` (an assignment, a coercion site's subtyping).
pub fn relate<A: Allocator + Copy>(cx: &mut Cx<'_, '_>, a: Arg, b: Arg, v: Variance, item: ItemKey, alloc: A) -> Result<(), Refused> {
    let mut envs: Envs<'_, A> = Envs::new_in(alloc);
    relate_sides(cx, &mut envs, item, Side::caller(a), Side::caller(b), v)
}

/// The well-formedness of each of `inst`'s inputs and its output at the call ([`Walk::wf`]):
/// a call's instance of `&'a &'b T` makes `'b: 'a` hold, as the declaration's implied bound
/// promised the callee.
pub fn well_formed<'c, 'k: 'c, A: Allocator + Copy>(cx: &mut Cx<'k, '_>, inst: &mut Inst<'c, A>) -> Result<(), Refused> {
    let item = inst.item;
    let mut w = walker(cx, &mut inst.envs, item);
    for &input in inst.inputs.iter() {
        w.wf(input, 0)?;
    }
    w.wf(inst.output, 0)
}

/// The instantiated clauses of `inst` turned into outlives constraints.
pub fn clauses<'c, 'k: 'c, A: Allocator + Copy>(cx: &mut Cx<'k, '_>, inst: &mut Inst<'c, A>) -> Result<(), Refused> {
    let item = inst.item;
    let mut w = walker(cx, &mut inst.envs, item);
    for c in inst.clauses.iter() {
        match *c {
            Clause::Outlives { long, short, at } => w.outlives(at, long, short)?,
            Clause::TypeOutlives { subject, region } => w.components(subject, region, 0)?,
            Clause::Trait { subject, trait_ref } => w.tie(subject, trait_ref)?,
            Clause::TraitOf { trait_ref } => w.tie_of(trait_ref)?,
            Clause::ProjOf { proj } => w.proj_of(proj)?,
        }
    }
    Ok(())
}
