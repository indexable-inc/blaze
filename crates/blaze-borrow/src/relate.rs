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
use blaze_own::Vec;
use blaze_typeck::region_facts::Outlives;
use blaze_typeck::regions::Region;
use blaze_typeck::{ItemKey, PNode, PTy};

use crate::inst::{Arg, Ask, AskArgs, Clause, EnvId, Envs, ImplValue, Inst, Keyed, RegAt, RpititTarget, Selected, Selection, Side, Slots, TypeAt, Typed};
use crate::types::{Kids, Pos, RegionTypes, SlotCount, Unknown};
use crate::{Bad, Head, NotSelected, Refused, Reg, ShapeWhy, Variance};

/// Bounds for supertrait elaboration in `elaborate_outlives_from_trait`, `elaborate_for_trait`,
/// and `elaborate_for_proj`: large trait hierarchies (e.g. reth's `FullProvider`) exceed 64
/// transitive supertraits.
const MAX_ELAB_VISITED: usize = 256;
const MAX_ELAB_STACK: usize = 512;
const MAX_ELAB_DEPTH: u32 = 24;
const MOST_IMPL_SEEN: usize = 4096;
type ElabKey = (ItemKey, PTy, bool);
const ELAB_KEY_ZERO: ElabKey = (ItemKey(0), PTy(0), false);

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
    /// A canonical rigid placeholder for the `ordinal`-th distinct higher-ranked binder variable
    /// (`0, 1, ...` by first appearance inside a `dyn for<..>` trait object at invariant position).
    /// Sharing by `ordinal` across calls in one body lets a local `dyn for<'a> Tr<'a>` be passed
    /// by `&mut` to multiple calls while still rejecting caller universals, `'static`, local
    /// borrows, and mismatched binder arity (`for<'a, 'b>` vs `for<'a>`).
    fn dyn_placeholder(&mut self, _ordinal: u32) -> Origin {
        self.fresh(Kind::Placeholder)
    }
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
    /// The caller's own signature column (`FnPtr([inputs.., output])`), in the caller's
    /// numbering (`owner_env`), for RFC 1214 implied outlives bounds (`&'a T::Assoc` in the
    /// signature implies `T::Assoc: 'a`).
    fn sig(&self) -> Option<Typed<'_>> {
        None
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
pub const MOST_WALK_STEPS: u32 = 1 << 20;
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
    /// Guard against re-entrant alias normalization inside [`Walk::erased_sides_norm`].
    in_erased_norm: bool,
    /// Active and already-proven `(imp, env)` pairs whose impl parameters are all bound,
    /// preventing coinductive self-referential `impl_where` cycles and redundant re-proofs
    /// across giant trait stacks.
    impl_seen: &'w mut Vec<(ItemKey, EnvId), A>,
    walk_stack: [Option<WalkFrame<'c>>; WALK_STACK_CAP],
    walk_sp: usize,
    comp_stack: [Option<CompFrame<'c>>; COMP_STACK_CAP],
    comp_sp: usize,
}

/// Maximum number of trait/projection arguments cached in normalized form for [`AskAt`].
const MAX_NORM_ARGS: usize = 8;

/// The ask's arguments `[Self, trait args..]`, where a side holds them.
#[derive(Clone, Copy)]
enum AskAt<'c> {
    /// An alias, or a bound laid subject-first (`ExTrait(Tr, [T, A..])`): its children.
    Kids(Side<'c>),
    /// A bound with its subject apart (`ExTrait(Tr, [A..])`): the subject, then the children
    /// (with any alias argument `1..MAX_NORM_ARGS` normalized into `norm[k]` when `Some`).
    Subject { subject: Side<'c>, of: Side<'c>, norm: [Option<Side<'c>>; MAX_NORM_ARGS] },
    /// Subject-first node (`ExTrait`, `Alias`, or `ExProj` of `len` args) whose child 0 has
    /// already been normalized to `subject`, with remaining arguments `1..len` from `of`
    /// (with any alias argument `1..MAX_NORM_ARGS` normalized into `norm[k]` when `Some`).
    SubjectKids { subject: Side<'c>, of: Side<'c>, len: u32, norm: [Option<Side<'c>>; MAX_NORM_ARGS] },
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
            AskAt::Subject { of, norm, .. } => {
                if let Some(Some(s)) = norm.get(k as usize).copied() {
                    Some(s)
                } else {
                    self.walk.erased_kid(of, k - 1)
                }
            }
            AskAt::SubjectKids { subject, .. } if k == 0 => Some(subject),
            AskAt::SubjectKids { of, len, norm, .. } if k < len => {
                if let Some(Some(s)) = norm.get(k as usize).copied() {
                    Some(s)
                } else {
                    self.walk.erased_kid(of, k)
                }
            }
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

/// Outcome of attempting to normalize a trait fn's RPITIT opaque ([`Walk::normalize_rpitit`]).
enum RpititStep<'c> {
    /// Not an RPITIT opaque or no concrete impl was selected.
    None,
    /// Provided default (`rpitit::Twin`): the other side is `Opaque(twin, ..)` with identical
    /// captured lifetimes and type arguments.
    Twin,
    /// Concrete impl fn's return subtree at the opaque's place, in the impl fn's environment.
    Normalized(Side<'c>),
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
            Slots::Caller(_) | Slots::CallerEnv { .. } | Slots::CallerFresh { .. } => self.types,
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
                let r = *usize::try_from(k).ok().and_then(|i| col.get(i)).ok_or(self.malformed(s.ty, Bad::StepBound))?;
                self.envs.reg(env, r).map_err(|w| self.malformed(s.ty, w))
            }
            Slots::CallerFresh { env, start, len } => {
                if k >= len.get() {
                    return Err(self.malformed(s.ty, Bad::StepBound));
                }
                let idx = start.checked_add(k).ok_or(self.malformed(s.ty, Bad::StepBound))?;
                self.envs.reg(env, Region::Param(idx)).map_err(|w| self.malformed(s.ty, w))
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

    fn hr_dyn_origin(&mut self, node: PTy, key: BoundAt) -> Result<Origin, Refused> {
        let mut ordinal = 0u32;
        if let Some(used) = self.hr.get(..self.hr_len) {
            for &(k, o) in used.iter().flatten() {
                if k == key {
                    return Ok(o);
                }
                if k.env == key.env && k.binder == key.binder {
                    ordinal += 1;
                }
            }
        }
        let Some(slot) = self.hr.get_mut(self.hr_len) else {
            return Err(self.malformed(node, Bad::StepBound));
        };
        let o = self.out.dyn_placeholder(ordinal);
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
            let key = BoundAt { env: ea, binder, var };
            let o = match self.binder_v {
                BinderCtx::FnPtr(Variance::Co) | BinderCtx::Dynamic(Variance::Co) | BinderCtx::Clause => self.hr_origin(node, key, Kind::Existential)?,
                BinderCtx::FnPtr(Variance::Contra) => self.hr_origin(node, key, Kind::Placeholder)?,
                BinderCtx::Dynamic(Variance::Contra | Variance::In) if eb.is_none() => self.hr_dyn_origin(node, key)?,
                BinderCtx::Dynamic(Variance::Contra) => self.hr_origin(node, key, Kind::Placeholder)?,
                BinderCtx::None | BinderCtx::FnPtr(Variance::In) | BinderCtx::Dynamic(Variance::In) => {
                    return Err(Refused::HigherRanked { item: self.item, node, binder });
                }
            };
            a = Reg::Origin(o);
        }
        if let Reg::Bound { binder, var } = b {
            let key = BoundAt { env: eb, binder, var };
            let o = match self.binder_v {
                BinderCtx::FnPtr(Variance::Co) | BinderCtx::Clause => self.hr_origin(node, key, Kind::Placeholder)?,
                BinderCtx::Dynamic(Variance::Co | Variance::In) if ea.is_none() => self.hr_dyn_origin(node, key)?,
                BinderCtx::Dynamic(Variance::Co) => self.hr_origin(node, key, Kind::Placeholder)?,
                BinderCtx::FnPtr(Variance::Contra) | BinderCtx::Dynamic(Variance::Contra) => self.hr_origin(node, key, Kind::Existential)?,
                BinderCtx::None | BinderCtx::FnPtr(Variance::In) | BinderCtx::Dynamic(Variance::In) => {
                    return Err(Refused::HigherRanked { item: self.item, node, binder });
                }
            };
            b = Reg::Origin(o);
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
            Slots::Caller(_) | Slots::CallerFresh { .. } => None,
            Slots::Sig { env, .. } | Slots::CallerEnv { env, .. } => Some(env),
        }
    }

    /// Structural erased equality between two resolved sides (including `Alias`, `Opaque`,
    /// `Param` heads): used to match caller/trait/opaque where-clauses against an asked subject
    /// and trait arguments. When the fast syntactic check returns `false` and either side holds
    /// an alias (e.g. a forwarding inner alias like `<<TempoEthApi<N> as RpcNodeCore>::Evm as
    /// ConfigureEvm>::BlockExecutorFactory` vs `<<N as RpcNodeCore>::Evm as ConfigureEvm>::BlockExecutorFactory`),
    /// retries with inner-alias normalization (`d > 0`, preserving `walk_loop`'s `same_alias`).
    fn erased_sides(&mut self, a: Side<'c>, b: Side<'c>) -> bool {
        if self.erased_sides_raw(a, b) {
            return true;
        }
        if self.in_erased_norm {
            return false;
        }
        self.erased_sides_norm(a, b, false, false)
    }

    /// Match a non-self clause argument `clause_arg` against a goal argument `goal_arg`:
    /// tries fast syntactic equality first, then alias-normalizing equality including top-level
    /// (`d >= 0`) aliases; when `allow_alias` is true, permits an unnormalized `PNode::Alias`
    /// only where `walk_loop` under `BinderCtx::Clause` soundly relates it without failing
    /// ([`Walk::unnorm_alias_matches`]).
    fn match_clause_arg(&mut self, clause_arg: Side<'c>, goal_arg: Side<'c>, allow_alias: bool) -> bool {
        if self.erased_sides_raw(clause_arg, goal_arg) {
            return true;
        }
        if self.in_erased_norm {
            return false;
        }
        self.erased_sides_norm(clause_arg, goal_arg, allow_alias, true)
    }

    /// Whether an unnormalized `PNode::Alias` on one side may match the other side during the
    /// `allow_alias = true` pass of clause-argument matching. Mirrors the exact cases where
    /// `walk_loop` under `BinderCtx::Clause` succeeds without normalizing the alias:
    /// - `a` (clause side) is `Alias` and `b` (goal side) is inert or `a` is a zero-slot builtin
    ///   associated type (`Pointee::Metadata`, `DiscriminantKind::Discriminant`);
    /// - `b` (goal side) is `Alias` and both `a` and `b` are inert or `b` is a zero-slot builtin
    ///   associated type.
    ///
    /// A rigid non-inert goal alias like `<S as ClosureSource>::Error` MUST NOT match a concrete
    /// clause argument like `CheckError` (which would falsely commit to `S::Error: From<CheckError>`
    /// when proving `S::Error: From<S::Error>`, aborting before blanket `impl<T> From<T> for T`).
    fn unnorm_alias_matches(&self, a: Side<'c>, na: PNode, b: Side<'c>, nb: PNode) -> bool {
        if let PNode::Alias { trait_, name, .. } = na {
            if self.selected.is_zero_slot_builtin_assoc(trait_, name) {
                return true;
            }
            let mut budget = MOST_WALK_STEPS;
            if matches!(self.inert(b, 0, &mut budget), Ok(true)) {
                return true;
            }
        }
        if let PNode::Alias { trait_, name, .. } = nb {
            if self.selected.is_zero_slot_builtin_assoc(trait_, name) {
                return true;
            }
            let mut ba = MOST_WALK_STEPS;
            let mut bb = MOST_WALK_STEPS;
            if matches!(self.inert(a, 0, &mut ba), Ok(true)) && matches!(self.inert(b, 0, &mut bb), Ok(true)) {
                return true;
            }
        }
        false
    }

    fn erased_sides_raw(&self, a: Side<'c>, b: Side<'c>) -> bool {
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

    /// Alias-normalizing erased structural equality: descends with split slots where available and
    /// normalizes `PNode::Alias` nodes before failing `head_eq` or recursing on differing alias
    /// arguments. Reclaims any temporary environments created during normalization.
    fn erased_sides_norm(&mut self, a: Side<'c>, b: Side<'c>, allow_alias: bool, allow_top_norm: bool) -> bool {
        const MAX_FRAMES: usize = 64;
        #[derive(Clone, Copy)]
        struct NormFrame<'c> {
            a: Side<'c>,
            b: Side<'c>,
            a_rest: Option<Slots<'c>>,
            b_rest: Option<Slots<'c>>,
            next_k: u32,
            depth: u32,
        }
        let saved = (self.bound, self.bound_len, self.hr, self.hr_len, self.binder_v);
        let saved_budget = self.budget;
        self.budget = self.budget.min(4096);
        let env_mark = self.envs.len();
        let seen_mark = self.impl_seen.len();
        self.in_erased_norm = true;
        let mut stack: [Option<NormFrame<'c>>; MAX_FRAMES] = [None; MAX_FRAMES];
        let mut sp = 0usize;
        let mut budget = 1024u32;
        let mut enter = Some((a, b, 0u32));
        let res;
        loop {
            if let Some((a, b, d)) = enter.take() {
                if d >= 32 || budget == 0 {
                    res = false;
                    break;
                }
                budget -= 1;
                let (Ok(Resolved::Side(a)), Ok(Resolved::Side(b))) = (self.resolve(a), self.resolve(b)) else {
                    res = false;
                    break;
                };
                if self.erased_sides_raw(a, b) {
                    continue;
                }
                let (Ok(na), Ok(nb)) = (self.node(a), self.node(b)) else {
                    res = false;
                    break;
                };
                if is_const(na) && is_const(nb) {
                    continue;
                }
                if (d > 0 || allow_top_norm) && (matches!(na, PNode::Alias { .. }) || matches!(nb, PNode::Alias { .. })) {
                    let (a2, da) = if matches!(na, PNode::Alias { .. }) && self.fits(a).is_ok() {
                        self.try_normalize_subject_flag(a, d)
                    } else {
                        (a, false)
                    };
                    let (b2, db) = if matches!(nb, PNode::Alias { .. }) && self.fits(b).is_ok() {
                        self.try_normalize_subject_flag(b, d)
                    } else {
                        (b, false)
                    };
                    if da || db {
                        enter = Some((a2, b2, d + 1));
                        continue;
                    }
                }
                if allow_alias && self.unnorm_alias_matches(a, na, b, nb) {
                    continue;
                }
                if !head_eq(na, nb) {
                    res = false;
                    break;
                }
                let (Ok(ka), Ok(kb)) = (self.kids_of(a), self.kids_of(b)) else {
                    res = false;
                    break;
                };
                if ka.len() != kb.len() {
                    res = false;
                    break;
                }
                if ka.len() > 0 {
                    let a_rest = if self.fits(a).is_ok() {
                        self.own(a).ok().and_then(|o| a.slots.split(o)).map(|(_, r)| r)
                    } else {
                        None
                    };
                    let b_rest = if self.fits(b).is_ok() {
                        self.own(b).ok().and_then(|o| b.slots.split(o)).map(|(_, r)| r)
                    } else {
                        None
                    };
                    let Some(slot) = stack.get_mut(sp) else {
                        res = false;
                        break;
                    };
                    *slot = Some(NormFrame { a, b, a_rest, b_rest, next_k: 0, depth: d });
                    sp += 1;
                }
                continue;
            }
            let Some(top_idx) = sp.checked_sub(1) else {
                res = true;
                break;
            };
            let Some(Some(mut frame)) = stack.get(top_idx).copied() else {
                res = false;
                break;
            };
            let (Ok(ka), Ok(kb)) = (self.kids_of(frame.a), self.kids_of(frame.b)) else {
                res = false;
                break;
            };
            if frame.next_k < ka.len() {
                let k = frame.next_k;
                frame.next_k = k + 1;
                let a_kid = if let Some(ar) = frame.a_rest {
                    match self.kid(frame.a, &ka, ar, k) {
                        Ok((kid, next_r)) => {
                            frame.a_rest = Some(next_r);
                            kid
                        }
                        Err(_) => {
                            frame.a_rest = None;
                            let Some(x) = ka.get(k) else { res = false; break };
                            Side { ty: x, slots: frame.a.slots }
                        }
                    }
                } else {
                    let Some(x) = ka.get(k) else { res = false; break };
                    Side { ty: x, slots: frame.a.slots }
                };
                let b_kid = if let Some(br) = frame.b_rest {
                    match self.kid(frame.b, &kb, br, k) {
                        Ok((kid, next_r)) => {
                            frame.b_rest = Some(next_r);
                            kid
                        }
                        Err(_) => {
                            frame.b_rest = None;
                            let Some(y) = kb.get(k) else { res = false; break };
                            Side { ty: y, slots: frame.b.slots }
                        }
                    }
                } else {
                    let Some(y) = kb.get(k) else { res = false; break };
                    Side { ty: y, slots: frame.b.slots }
                };
                if let Some(slot) = stack.get_mut(top_idx) {
                    *slot = Some(frame);
                }
                enter = Some((a_kid, b_kid, frame.depth + 1));
            } else {
                sp = top_idx;
            }
        }
        self.in_erased_norm = false;
        self.impl_seen.truncate(seen_mark);
        self.truncate_envs(env_mark);
        self.budget = saved_budget.saturating_sub(1);
        (self.bound, self.bound_len, self.hr, self.hr_len, self.binder_v) = saved;
        res
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
            /// Inside a matched `Alias` pair, a child mismatch means `Unknown` (the two aliases
            /// may still normalize to the same type), never `Differ`.
            in_alias: bool,
        }
        let open = |n: PNode| matches!(n, PNode::Infer | PNode::Error | PNode::Alias { .. });
        let mut stack: [Option<ErasedFrame<'c>>; MAX_FRAMES] = [None; MAX_FRAMES];
        let mut sp: usize = 0;
        let mut out = Keyed::Same;
        let mut enter = Some((a, b, depth, false));
        loop {
            if let Some((a, b, d, in_alias)) = enter.take() {
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
                if matches!(a.slots, Slots::Caller(_) | Slots::CallerFresh { .. })
                    && a.ty == b
                {
                    continue;
                }
                let (Ok(na), Ok(nb)) = (self.node(a), self.types.node(b)) else {
                    out = Keyed::Unknown;
                    continue;
                };
                // Two syntactically identical aliases (`<T as Tr>::A` vs `<T as Tr>::A`, e.g.
                // inside `NodeTypesWithDBAdapter<Node::Types, Node::DB>`) match `Keyed::Same` when
                // all their arguments match `Keyed::Same`; any mismatch inside an alias downgrades
                // to `Keyed::Unknown` (never `Differ`, since distinct aliases can normalize equal).
                let same_alias_head = matches!(
                    (na, nb),
                    (PNode::Alias { trait_: t1, name: n1, .. }, PNode::Alias { trait_: t2, name: n2, .. })
                        if t1 == t2 && n1 == n2
                );
                // What structure does not decide: a hole, an error, or an alias not normalized here.
                // Distinct `Opaque(x)` / `Opaque(y)` and `Opaque` vs concrete types are distinct
                // nominal types in callers (`head_eq` returns `Keyed::Differ`).
                if !same_alias_head && (open(na) || open(nb)) {
                    out = Keyed::Unknown;
                    continue;
                }
                // A const's type is a node of its own table: compare its bits, then its type.
                if let (PNode::ConstValue { ty: ta, bits: x }, PNode::ConstValue { ty: tb, bits: y }) = (na, nb) {
                    if x != y {
                        if in_alias {
                            out = Keyed::Unknown;
                            continue;
                        }
                        return Keyed::Differ;
                    }
                    enter = Some((Side { ty: ta, slots: a.slots }, tb, d + 1, in_alias));
                    continue;
                }
                // An impl's or callee's const parameter (never bound by `blaze-borrow`, since
                // consts carry no region slots and `Array(c, _)` ignores its length) matches a
                // typeck row's const argument loosely (`Keyed::Unknown`), never `Differ`.
                if matches!(a.slots, Slots::Sig { .. }) && matches!(na, PNode::ConstParam(_)) && is_const(nb) {
                    out = Keyed::Unknown;
                    continue;
                }
                if !same_alias_head && !head_eq(na, nb) {
                    if in_alias {
                        out = Keyed::Unknown;
                        continue;
                    }
                    return Keyed::Differ;
                }
                let (Ok(ka), Ok(kb)) = (self.kids_of(a), self.types.kids(b)) else {
                    out = Keyed::Unknown;
                    continue;
                };
                if ka.len() != kb.len() {
                    if in_alias || same_alias_head {
                        out = Keyed::Unknown;
                        continue;
                    }
                    return Keyed::Differ;
                }
                if ka.len() > 0 {
                    let Some(slot) = stack.get_mut(sp) else {
                        out = Keyed::Unknown;
                        continue;
                    };
                    *slot = Some(ErasedFrame { a, b, next_k: 0, depth: d, in_alias: in_alias || same_alias_head });
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
                enter = Some((Side { ty: x, slots: frame.a.slots }, y, frame.depth + 1, frame.in_alias));
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
                // A trait fn's RPITIT return type appears as `Opaque(op, args)` in the trait
                // signature and as its synthetic `Alias { trait_, name, args }` in the caller's
                // typed body. When delegating through a caller where-clause (no concrete impl
                // selected), both sides denote the same abstract future and equate directly.
                let same_rpitit = match (na, nb) {
                    (PNode::Opaque(op, _), PNode::Alias { trait_, name, .. })
                    | (PNode::Alias { trait_, name, .. }, PNode::Opaque(op, _)) => self.selected.is_rpitit_alias(op, trait_, name),
                    _ => false,
                };
                if !same_alias && !same_rpitit {
                    if matches!(na, PNode::Alias { .. }) || matches!(nb, PNode::Alias { .. }) {
                        let mut ba = MOST_WALK_STEPS;
                        let mut bb = MOST_WALK_STEPS;
                        if self.inert(a, 0, &mut ba)? && self.inert(b, 0, &mut bb)? {
                            continue;
                        }
                        if self.fits(a)? == SlotCount::ZERO && self.fits(b)? == SlotCount::ZERO {
                            let mut ba = MOST_WALK_STEPS;
                            let mut bb = MOST_WALK_STEPS;
                            if self.inert_norm(a, 0, &mut ba)? && self.inert_norm(b, 0, &mut bb)? {
                                continue;
                            }
                        }
                    }
                    if let PNode::Alias { trait_, name, .. } = na {
                        if self.selected.is_zero_slot_builtin_assoc(trait_, name) {
                            let mut budget = MOST_WALK_STEPS;
                            if self.inert(b, 0, &mut budget)? {
                                continue;
                            }
                            if self.pointee_dyn_metadata(a, b, trait_, name, true, v, bv, d)? {
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
                        } else if bv == BinderCtx::Clause {
                            // Inside `BinderCtx::Clause`, `a` comes from a caller's already-assumed
                            // clause (or its supertrait elaboration), whose impl well-formedness is
                            // guaranteed by the caller's environment. If its impl was not recorded
                            // at this call site and `b` is completely region- and param-free (`inert`),
                            // equating `a` with `b` cannot constrain any region.
                            let saved = (self.bound, self.bound_len, self.hr, self.hr_len, self.binder_v);
                            match self.normalize(a, na, d) {
                                Ok(a_norm) => {
                                    enter = Some((a_norm, b, v, bv, d + 1));
                                    continue;
                                }
                                Err(Refused::Unselected { .. }) => {
                                    (self.bound, self.bound_len, self.hr, self.hr_len, self.binder_v) = saved;
                                    let mut bb = MOST_WALK_STEPS;
                                    if self.inert(b, 0, &mut bb)? {
                                        continue;
                                    }
                                    return self.normalize(a, na, d).map(|_| ());
                                }
                                Err(err) => return Err(err),
                            }
                        } else {
                            let saved = (self.bound, self.bound_len, self.hr, self.hr_len, self.binder_v);
                            match self.normalize(a, na, d) {
                                Ok(a_norm) => {
                                    enter = Some((a_norm, b, v, bv, d + 1));
                                    continue;
                                }
                                Err(Refused::Unselected { why: NotSelected::GenericSelf(crate::inst::SelfHead::Other), .. })
                                    if self.ground_where_gat_bridge(a, b, trait_, v)? =>
                                {
                                    (self.bound, self.bound_len, self.hr, self.hr_len, self.binder_v) = saved;
                                    continue;
                                }
                                Err(err) => return Err(err),
                            }
                        }
                    }
                    if let PNode::Alias { trait_, name, .. } = nb {
                        if self.selected.is_zero_slot_builtin_assoc(trait_, name) {
                            let mut budget = MOST_WALK_STEPS;
                            if self.inert(a, 0, &mut budget)? {
                                continue;
                            }
                            if self.pointee_dyn_metadata(b, a, trait_, name, false, v, bv, d)? {
                                continue;
                            }
                        }
                        let b = self.normalize(b, nb, d)?;
                        enter = Some((a, b, v, bv, d + 1));
                        continue;
                    }
                }
                let mut heads_match = head_eq(na, nb) || same_rpitit;
                if !heads_match {
                    if let PNode::Opaque(op_key, _) = na {
                        match self.normalize_rpitit(a, op_key, nb, d)? {
                            RpititStep::Twin => heads_match = true,
                            RpititStep::Normalized(a_norm) => {
                                enter = Some((a_norm, b, v, bv, d + 1));
                                continue;
                            }
                            RpititStep::None => {}
                        }
                    }
                    if !heads_match
                        && let PNode::Opaque(op_key, _) = nb
                    {
                        match self.normalize_rpitit(b, op_key, na, d)? {
                            RpititStep::Twin => heads_match = true,
                            RpititStep::Normalized(b_norm) => {
                                enter = Some((a, b_norm, v, bv, d + 1));
                                continue;
                            }
                            RpititStep::None => {}
                        }
                    }
                }
                if !heads_match {
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
    fn try_normalize_subject(&mut self, s: Side<'c>, depth: u32) -> Side<'c> {
        self.try_normalize_subject_flag(s, depth).0
    }

    /// Normalizes alias `s` through selected impls/bounds, also returning whether at least one
    /// normalization step occurred. Tracking this flag (rather than comparing `PTy` alone) is
    /// required because `<P as Tr>::Assoc` in a callee env (`P -> &Client`) and `<T as Tr>::Assoc`
    /// in `impl<T: Tr> Tr for &T` (`T -> Client`) share the same interned `PTy` index in `sig_types`
    /// while carrying different `EnvId`s.
    fn try_normalize_subject_flag(&mut self, mut s: Side<'c>, depth: u32) -> (Side<'c>, bool) {
        if depth >= MOST_WALK_DEPTH / 2 {
            return (s, false);
        }
        let mut normalized = false;
        for step in 0..8u32 {
            let Ok(Resolved::Side(cur)) = self.resolve(s) else { break };
            s = cur;
            let Ok(n @ PNode::Alias { .. }) = self.node(s) else { break };
            let saved = (self.bound, self.bound_len, self.hr, self.hr_len, self.binder_v);
            let env_mark = self.envs.len();
            let seen_mark = self.impl_seen.len();
            match self.normalize(s, n, depth + step + 1) {
                Ok(next) => {
                    (self.bound, self.bound_len, self.hr, self.hr_len, self.binder_v) = saved;
                    s = next;
                    normalized = true;
                }
                Err(_) => {
                    (self.bound, self.bound_len, self.hr, self.hr_len, self.binder_v) = saved;
                    self.impl_seen.truncate(seen_mark);
                    self.truncate_envs(env_mark);
                    break;
                }
            }
        }
        (s, normalized)
    }

    /// The selected impl's (or bound's / builtin callable's) value for the alias `s` (its node `n`).
    fn normalize(&mut self, s: Side<'c>, n: PNode, depth: u32) -> Result<Side<'c>, Refused> {
        let PNode::Alias { trait_, name, .. } = n else { return Err(self.malformed(s.ty, Bad::NotOfKind)) };
        self.fits(s)?;
        let own = self.own(s)?;
        let kids = self.kids_of(s)?;
        let (_, args_rest) = s.slots.split(own).ok_or(self.malformed(s.ty, Bad::StepBound))?;
        let mut norm_subject = None;
        let mut raw_fallback = None;
        let mut norm_args = [None; MAX_NORM_ARGS];
        if kids.len() > 0 {
            let (raw_sub, mut rem) = self.kid(s, &kids, args_rest, 0)?;
            if let Resolved::Side(raw_sub) = self.resolve(raw_sub)? {
                let (subject, changed) = self.try_normalize_subject_flag(raw_sub, depth);
                norm_subject = Some(subject);
                if let Some(val) = self.try_normalize_non_impl(s, kids.len(), subject, trait_, name)? {
                    return Ok(val);
                }
                if changed || subject.ty != raw_sub.ty {
                    raw_fallback = Some(raw_sub);
                }
            }
            for k in 1..kids.len() {
                let (arg, r) = self.kid(s, &kids, rem, k)?;
                rem = r;
                if let Some(slot) = norm_args.get_mut(k as usize)
                    && let Resolved::Side(arg_s) = self.resolve(arg)?
                {
                    let (n, changed) = self.try_normalize_subject_flag(arg_s, depth);
                    if changed {
                        *slot = Some(n);
                    }
                }
            }
        }
        let selected: &'c dyn Selected = self.selected;
        let at = match norm_subject {
            Some(subject) => AskAt::SubjectKids { subject, of: s, len: kids.len(), norm: norm_args },
            None => AskAt::Kids(s),
        };
        let key = Keyer { walk: &*self, at, budget: Cell::new(MOST_WALK_STEPS) };
        let (head, value) = match selected.select_keyed(Ask { trait_, name: Some(name), written: s.ty }, &key) {
            Selection::Impl { head, value: Some(value) } => (head, value),
            Selection::Impl { value: None, .. } => return Err(self.malformed(s.ty, Bad::NoValue)),
            Selection::Not(why) => {
                // Only fall back to non-impl bounds on the unnormalized raw subject when no
                // concrete impl was selected for the normalized subject. Checking raw-subject
                // trait bounds before `select_keyed` would prefer a cyclic trait bound like
                // `SolCall::Parameters<'a>: SolType<Token<'a> = Self::Token<'a>>` over the
                // concrete `impl SolType for (Address, Uint<256>)`, looping forever until
                // `MOST_WALK_DEPTH`.
                if let Some(raw_sub) = raw_fallback
                    && let Some(val) = self.try_normalize_non_impl(s, kids.len(), raw_sub, trait_, name)?
                {
                    return Ok(val);
                }
                return Err(Refused::Unselected { item: self.item, node: s.ty, trait_, name: Some(name), why });
            }
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
        let (_, rest) = s.slots.split(own).ok_or(self.malformed(s.ty, Bad::StepBound))?;
        let (_, hrest) = header.slots.split(tl).ok_or(self.malformed(s.ty, Bad::StepBound))?;
        self.walk_header_kids(header, &hkids, hrest, s, &kids, rest, None, true, depth + 1)?;
        self.impl_where(head.imp, trait_, s.ty, env, head.lifetimes)?;
        Ok(Side { ty: value.ty, slots: Slots::Sig { col: value.col, env } })
    }

    /// Whether the syntactic type tree under `s` (in `s`'s own table, without normalizing) holds
    /// any `PNode::Alias`. Used by [`Walk::walk_header_kids`] to defer alias-containing impl
    /// header arguments until non-alias arguments have bound the impl's type parameters.
    fn has_alias(&self, s: Side<'c>) -> bool {
        let tab = self.tab(s.slots);
        let mut stack = [PTy(0); 64];
        let mut sp = 0usize;
        stack[0] = s.ty;
        sp += 1;
        let mut budget = 256u32;
        while sp > 0 && budget > 0 {
            sp -= 1;
            budget -= 1;
            let ty = stack[sp];
            let Ok(node) = tab.node(ty) else { continue };
            if matches!(node, PNode::Alias { .. }) {
                return true;
            }
            if let Ok(kids) = tab.kids(ty) {
                for k in 0..kids.len() {
                    if let Some(kid) = kids.get(k)
                        && sp < stack.len()
                    {
                        stack[sp] = kid;
                        sp += 1;
                    }
                }
            }
        }
        false
    }

    /// Relate an impl header's type arguments `[SelfTy, A..]` against the goal's arguments
    /// invariantly, walking alias-free header arguments before alias-containing ones so impl
    /// parameters appearing after a projection (e.g. `impl<'a, T: AsContextMut> From<&'a mut T>
    /// for StoreContextMut<'a, T::Data>`) are bound before the projection normalizes.
    #[allow(clippy::too_many_arguments)]
    fn walk_header_kids(
        &mut self,
        header: Side<'c>,
        hkids: &Kids<'c>,
        hrest0: Slots<'c>,
        other: Side<'c>,
        okids: &Kids<'c>,
        orest0: Slots<'c>,
        subject: Option<Side<'c>>,
        other_has_subject: bool,
        depth: u32,
    ) -> Result<(), Refused> {
        let passes: &[Option<bool>] = if hkids.len() > 1 { &[Some(false), Some(true)] } else { &[None] };
        for &want_alias in passes {
            let mut hrest = hrest0;
            let mut orest = orest0;
            for k in 0..hkids.len() {
                let (h, hr) = self.kid(header, hkids, hrest, k)?;
                hrest = hr;
                let rhs = if k == 0 && let Some(subj) = subject {
                    if other_has_subject {
                        let (_, r) = self.kid(other, okids, orest, 0)?;
                        orest = r;
                    }
                    subj
                } else {
                    let ok = if other_has_subject { k } else { k - 1 };
                    let (x, r) = self.kid(other, okids, orest, ok)?;
                    orest = r;
                    x
                };
                if want_alias.is_none_or(|w| self.has_alias(h) == w) {
                    self.walk(h, rhs, Variance::In, depth)?;
                }
            }
        }
        Ok(())
    }

    /// Builtin `<dyn Trait + 'a as Pointee>::Metadata` <-> `DynMetadata<dyn Trait + 'b>`:
    /// `DynMetadata<Dyn>` is covariant in `Dyn` (`PhantomData<Dyn>`), so relating the
    /// projection against `DynMetadata<D>` under variance `v` relates `subject` against `D`
    /// under `v` (subtype/supertype order preserved by `alias_is_sub`).
    #[allow(clippy::too_many_arguments)]
    fn pointee_dyn_metadata(
        &mut self,
        alias: Side<'c>,
        other: Side<'c>,
        trait_: ItemKey,
        name: blaze_typeck::Name,
        alias_is_sub: bool,
        v: Variance,
        bv: BinderCtx,
        depth: u32,
    ) -> Result<bool, Refused> {
        if !self.selected.is_zero_slot_builtin_assoc(trait_, name) || name != blaze_typeck::Name::of("Metadata") {
            return Ok(false);
        }
        if self.fits(alias).is_err() || self.fits(other).is_err() {
            return Ok(false);
        }
        let a_own = self.own(alias)?;
        let akids = self.kids_of(alias)?;
        if akids.len() == 0 {
            return Ok(false);
        }
        let Some((_, arest)) = alias.slots.split(a_own) else { return Ok(false) };
        let (subj, _) = self.kid(alias, &akids, arest, 0)?;
        let Resolved::Side(subj) = self.resolve(subj)? else { return Ok(false) };
        if !matches!(self.node(subj)?, PNode::Dynamic(_)) {
            return Ok(false);
        }
        if !matches!(self.node(other)?, PNode::Adt(..)) {
            return Ok(false);
        }
        let o_own = self.own(other)?;
        let okids = self.kids_of(other)?;
        if o_own != SlotCount::ZERO || okids.len() != 1 {
            return Ok(false);
        }
        let (dyn_arg, _) = self.kid(other, &okids, other.slots, 0)?;
        let Resolved::Side(dyn_arg) = self.resolve(dyn_arg)? else { return Ok(false) };
        if !matches!(self.node(dyn_arg)?, PNode::Dynamic(_)) {
            return Ok(false);
        }
        let (sub, sup) = if alias_is_sub { (subj, dyn_arg) } else { (dyn_arg, subj) };
        self.walk_with_binder(sub, sup, v, bv, depth + 1)?;
        Ok(true)
    }

    /// When a ground projection `<K as Tr>::Assoc<'g>` (`K` and all trait args completely
    /// region- and param-free) fails selection with `GenericSelf(Other)` strictly because the
    /// caller has a lifetime-free ground `where K: Tr` clause (e.g. `evm2_macros::instruction`
    /// emitting `where TempoEvmTypes: EvmTypesHost`), `blaze-typeck` already normalized `<K as
    /// Tr>::Assoc<'g>` to `b` via the unique impl. If `a` has at most one GAT lifetime and `b`
    /// has a matching single slot (or is inert), relate them directly.
    fn ground_where_gat_bridge(&mut self, a: Side<'c>, b: Side<'c>, trait_: ItemKey, v: Variance) -> Result<bool, Refused> {
        let Ok(a_total) = self.fits(a) else { return Ok(false) };
        let Ok(a_own) = self.own(a) else { return Ok(false) };
        if a_own.get() > 1 || a_total != a_own {
            return Ok(false);
        }
        let Ok(akids) = self.kids_of(a) else { return Ok(false) };
        let Some((_, mut arest)) = a.slots.split(a_own) else { return Ok(false) };
        for k in 0..akids.len() {
            let Ok((kid, r)) = self.kid(a, &akids, arest, k) else { return Ok(false) };
            arest = r;
            let mut budget = MOST_WALK_STEPS;
            if !self.inert(kid, 0, &mut budget)? {
                return Ok(false);
            }
        }
        // Confirm the caller actually has a matching lifetime-free ground `where K: Tr` clause.
        let owner: &'c dyn Owner = self.owner;
        let clauses = owner.clauses();
        if clauses.is_empty() {
            return Ok(false);
        }
        let Some(oenv) = self.envs.owner_env() else { return Ok(false) };
        let mut has_ground_clause = false;
        for cv in clauses {
            let c = Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env: oenv } };
            let Ok(PNode::ExTrait(ct, _)) = self.node(c) else { continue };
            if ct != trait_ || self.fits(c).is_err() {
                continue;
            }
            let Ok(c_own) = self.own(c) else { continue };
            let Ok(ckids) = self.kids_of(c) else { continue };
            if c_own != SlotCount::ZERO || ckids.len() != akids.len() {
                continue;
            }
            let mut all_same = true;
            for k in 0..akids.len() {
                let (Some(ca), Some(aa)) = (self.erased_kid(c, k), self.erased_kid(a, k)) else {
                    all_same = false;
                    break;
                };
                if !self.erased_sides(ca, aa) {
                    all_same = false;
                    break;
                }
            }
            if all_same {
                has_ground_clause = true;
                break;
            }
        }
        if !has_ground_clause {
            return Ok(false);
        }
        let Ok(b_total) = self.fits(b) else { return Ok(false) };
        if b_total == SlotCount::ZERO {
            let mut budget = MOST_WALK_STEPS;
            return self.inert(b, 0, &mut budget);
        }
        if a_own.get() == 1 && b_total.get() == 1 {
            let ra = self.slot(a, 0)?;
            let rb = self.slot(b, 0)?;
            self.pair(a.ty, ra, rb, v, Self::env_of(a), Self::env_of(b))?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Normalize a trait fn's RPITIT opaque (`Opaque(op_key, ..)`, RFC 3425) at a concrete call
    /// site to either its provided default `Twin` or the selected impl fn's return subtree.
    fn normalize_rpitit(&mut self, s: Side<'c>, op_key: ItemKey, other: PNode, depth: u32) -> Result<RpititStep<'c>, Refused> {
        let Slots::Sig { env: call_env, .. } = s.slots else { return Ok(RpititStep::None) };
        let selected: &'c dyn Selected = self.selected;
        let key = Keyer { walk: &*self, at: AskAt::Kids(s), budget: Cell::new(MOST_WALK_STEPS) };
        let Some(target) = selected.rpitit_target(op_key, &key) else {
            return Ok(RpititStep::None);
        };
        match target {
            RpititTarget::Twin { twin } => {
                if matches!(other, PNode::Opaque(k, _) if k == twin) {
                    Ok(RpititStep::Twin)
                } else {
                    Ok(RpititStep::None)
                }
            }
            RpititTarget::ImplFn { trait_, head, trait_types, impl_fn: _, impl_params, impl_col, trait_col } => {
                self.fits(s)?;
                let own = self.own(s)?;
                let s_kids = self.kids_of(s)?;
                let Some(fn_types) = s_kids.len().checked_sub(trait_types) else {
                    return Ok(RpititStep::None);
                };
                let Some(total_types) = head.types.checked_add(fn_types) else {
                    return Ok(RpititStep::None);
                };
                let impl_env = self.envs.impl_env(impl_params.count(), total_types).ok_or(self.malformed(s.ty, Bad::StepBound))?;
                let saved = (self.bound, self.bound_len, self.hr, self.hr_len, self.binder_v);
                self.bound_len = 0;
                self.hr_len = 0;
                self.binder_v = BinderCtx::None;
                let res = self.bind_rpitit_impl(s, own, &s_kids, call_env, impl_env, trait_, head, trait_types, fn_types, impl_params, impl_col, trait_col, op_key, depth);
                (self.bound, self.bound_len, self.hr, self.hr_len, self.binder_v) = saved;
                match res? {
                    Some(side) => Ok(RpititStep::Normalized(side)),
                    None => Ok(RpititStep::None),
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn bind_rpitit_impl(
        &mut self,
        s: Side<'c>,
        own: SlotCount,
        s_kids: &Kids<'c>,
        call_env: EnvId,
        impl_env: EnvId,
        trait_: ItemKey,
        head: crate::inst::ImplHead<'c>,
        trait_types: u32,
        fn_types: u32,
        impl_params: blaze_typeck::region_facts::ParamsView<'c>,
        impl_col: blaze_typeck::region_facts::ColView<'c>,
        trait_col: blaze_typeck::region_facts::ColView<'c>,
        op_key: ItemKey,
        depth: u32,
    ) -> Result<Option<Side<'c>>, Refused> {
        let header = Side { ty: head.trait_ref.ty, slots: Slots::Sig { col: head.trait_ref.col, env: impl_env } };
        self.fits(header)?;
        let tl = self.own(header)?;
        for k in 0..tl.get() {
            let x = self.slot(header, k)?;
            let y = self.envs.reg(call_env, Region::Param(k)).map_err(|w| self.malformed(s.ty, w))?;
            self.pair(s.ty, x, y, Variance::In, Self::env_of(header), Some(call_env))?;
        }
        let hkids = self.kids_of(header)?;
        if hkids.len() != trait_types {
            return Err(self.malformed(s.ty, Bad::HeaderKids { alias: trait_types, header: hkids.len() }));
        }
        let (_, mut srest) = s.slots.split(own).ok_or(self.malformed(s.ty, Bad::StepBound))?;
        let (_, mut hrest) = header.slots.split(tl).ok_or(self.malformed(header.ty, Bad::StepBound))?;
        for k in 0..trait_types {
            let (h, hr) = self.kid(header, &hkids, hrest, k)?;
            let (x, sr) = self.kid(s, s_kids, srest, k)?;
            hrest = hr;
            srest = sr;
            self.walk(h, x, Variance::In, depth + 1)?;
        }
        for k in 0..fn_types {
            let (x, sr) = self.kid(s, s_kids, srest, trait_types + k)?;
            srest = sr;
            self.envs.bind_type(impl_env, head.types + k, x).map_err(|w| self.malformed(s.ty, w))?;
        }
        let impl_sig_kids = self.sig_types.kids(impl_col.ty()).map_err(|u| self.count_err(u))?;
        let trait_sig_kids = self.sig_types.kids(trait_col.ty()).map_err(|u| self.count_err(u))?;
        let Some(n_in) = impl_sig_kids.len().checked_sub(1) else {
            return Ok(None);
        };
        if trait_sig_kids.len() != impl_sig_kids.len() {
            return Ok(None);
        }
        for k in 0..n_in {
            let (Some(i_ty), Some(t_ty), Some(i_sl), Some(t_sl)) = (impl_sig_kids.get(k), trait_sig_kids.get(k), impl_col.child(k), trait_col.child(k)) else {
                return Ok(None);
            };
            let i_side = Side { ty: i_ty, slots: Slots::Sig { col: i_sl, env: impl_env } };
            let t_side = Side { ty: t_ty, slots: Slots::Sig { col: t_sl, env: call_env } };
            self.walk(i_side, t_side, Variance::In, depth + 1)?;
        }
        self.impl_where(head.imp, trait_, s.ty, impl_env, head.lifetimes)?;
        for i in head.lifetimes..impl_params.count() {
            if let RegAt::Unbound { param, .. } = self.envs.reg(impl_env, Region::Param(i)).map_err(|w| self.malformed(s.ty, w))? {
                let o = self.out.fresh(Kind::Existential);
                self.envs.bind_reg(impl_env, param, Reg::Origin(o)).map_err(|w| self.malformed(s.ty, w))?;
            }
        }
        for &(long, short) in impl_params.outlives() {
            let (l, sh) = (self.known_in(impl_env, long, s.ty)?, self.known_in(impl_env, short, s.ty)?);
            self.outlives(s.ty, l, sh)?;
        }
        let (Some(i_out_ty), Some(t_out_ty), Some(i_out_sl)) = (impl_sig_kids.get(n_in), trait_sig_kids.get(n_in), impl_col.child(n_in)) else {
            return Ok(None);
        };
        let Some((val_ty, val_col)) = self.rpitit_place(t_out_ty, i_out_ty, i_out_sl, op_key)? else {
            return Ok(None);
        };
        Ok(Some(Side { ty: val_ty, slots: Slots::Sig { col: val_col, env: impl_env } }))
    }

    /// Find the subtree `(impl_ty, impl_col)` at the exact position `PNode::Opaque(op_key, _)`
    /// occupies in `trait_ty`, slicing `impl_col` in pre-order (iterative, bounded).
    fn rpitit_place(&self, trait_ty: PTy, impl_ty: PTy, impl_col: &'c [Region], op_key: ItemKey) -> Result<Option<(PTy, &'c [Region])>, Refused> {
        const CAP: usize = 64;
        let mut stack: [Option<(PTy, PTy, &'c [Region])>; CAP] = [None; CAP];
        let mut sp = 0usize;
        if let Some(slot) = stack.get_mut(sp) {
            *slot = Some((trait_ty, impl_ty, impl_col));
            sp += 1;
        }
        let mut steps = 0u32;
        while let Some(top_idx) = sp.checked_sub(1) {
            sp = top_idx;
            let Some(Some((t, i, col))) = stack.get(top_idx).copied() else {
                return Ok(None);
            };
            steps += 1;
            if steps > MOST_WALK_STEPS {
                return Err(self.malformed(t, Bad::StepBound));
            }
            let nt = self.sig_types.node(t).map_err(|u| self.count_err(u))?;
            if matches!(nt, PNode::Opaque(k, _) if k == op_key) {
                return Ok(Some((i, col)));
            }
            let ni = self.sig_types.node(i).map_err(|u| self.count_err(u))?;
            if !head_eq(nt, ni) {
                continue;
            }
            let tkids = self.sig_types.kids(t).map_err(|u| self.count_err(u))?;
            let ikids = self.sig_types.kids(i).map_err(|u| self.count_err(u))?;
            if tkids.len() != ikids.len() {
                continue;
            }
            let i_own = self.sig_types.own(i).map_err(|u| self.count_err(u))?;
            let Some((_, mut rest)) = col.split_at_checked(i_own.len()) else {
                return Ok(None);
            };
            for k in 0..tkids.len() {
                let (Some(tk), Some(ik)) = (tkids.get(k), ikids.get(k)) else {
                    return Ok(None);
                };
                let kc = self.sig_types.count(ik).map_err(|u| self.count_err(u))?;
                let Some((kcol, next_rest)) = rest.split_at_checked(kc.len()) else {
                    return Ok(None);
                };
                rest = next_rest;
                let Some(slot) = stack.get_mut(sp) else {
                    return Err(self.malformed(t, Bad::StepBound));
                };
                *slot = Some((tk, ik, kcol));
                sp += 1;
            }
        }
        Ok(None)
    }

    /// Every component of `s` outlives `region`.
    fn alias_side_eq(&mut self, a: Side<'c>, b: Side<'c>) -> bool {
        let Some(n) = a.slots.len() else { return false };
        b.slots.len() == Some(n)
            && self.erased_sides(a, b)
            && (0..n.get()).all(|k| {
                matches!(
                    (self.slot(a, k), self.slot(b, k)),
                    (Ok(RegAt::Known(ra)), Ok(RegAt::Known(rb))) if ra == rb
                )
            })
    }

    /// Whether `cur` (an unselected alias) is already assumed to outlive some region `rhs_reg`
    /// — from the associated type's own item bounds (`type Error: 'static`), the trait's clauses,
    /// an explicit caller `where <Alias>: 'rhs` clause (`owner.clauses()`), or an RFC 1214 implied
    /// bound (`&'rhs <Alias>` in `owner.sig()`) — that can prove `cur: region`. When satisfied,
    /// emits `rhs_reg: region` and returns `Ok(true)`.
    fn alias_assumed_outlives(&mut self, cur: Side<'c>, region: Reg) -> Result<bool, Refused> {
        let mut unique_rhs: Option<Option<Reg>> = None;
        let record_rhs = |unique: &mut Option<Option<Reg>>, r: Reg| match *unique {
            None => *unique = Some(Some(r)),
            Some(Some(prev)) if prev == r => {}
            _ => *unique = Some(None),
        };
        // 1. Associated type item bounds (`type Error: Error + Send + Sync + 'static`) and the
        //    trait's own clauses (`selected.assoc_clauses` / `selected.trait_clauses`).
        if let Ok(PNode::Alias { trait_: at, name, .. }) = self.node(cur)
            && self.fits(cur).is_ok()
        {
            let selected: &'c dyn Selected = self.selected;
            let s_own = self.own(cur)?;
            let skids = self.kids_of(cur)?;
            for source in [selected.assoc_clauses(at, name), selected.trait_clauses(at)] {
                let Some((at_lts, views)) = source else { continue };
                if views.is_empty() || !views.iter().any(|cv| cv.outlives.is_some() && cv.binders == 0) {
                    continue;
                }
                let env_mark = self.envs.len();
                let lts = at_lts.max(s_own.get());
                let Some(aenv) = self.envs.impl_env(lts, skids.len()) else { continue };
                let mut bound_ok = true;
                for k in 0..at_lts.min(s_own.get()) {
                    match self.slot(cur, k)? {
                        RegAt::Known(r) if self.envs.bind_reg(aenv, k, r).is_ok() => {}
                        _ => {
                            bound_ok = false;
                            break;
                        }
                    }
                }
                if !bound_ok {
                    self.truncate_envs(env_mark);
                    continue;
                }
                for k in at_lts.min(s_own.get())..lts {
                    let o = self.out.fresh(Kind::Existential);
                    let _ = self.envs.bind_reg(aenv, k, Reg::Origin(o));
                }
                let Some((_, mut srest)) = cur.slots.split(s_own) else {
                    self.truncate_envs(env_mark);
                    continue;
                };
                for k in 0..skids.len() {
                    let Ok((c, r)) = self.kid(cur, &skids, srest, k) else {
                        bound_ok = false;
                        break;
                    };
                    srest = r;
                    let _ = self.envs.bind_type(aenv, k, c);
                }
                if !bound_ok {
                    self.truncate_envs(env_mark);
                    continue;
                }
                for cv in views {
                    let Some(rhs) = cv.outlives else { continue };
                    if cv.binders != 0 {
                        continue;
                    }
                    let c = Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env: aenv } };
                    if !self.alias_side_eq(c, cur) {
                        continue;
                    }
                    let Ok(RegAt::Known(rhs_reg)) = self.envs.reg(aenv, rhs) else { continue };
                    if rhs_reg == Reg::Static || rhs_reg == region {
                        self.outlives(cur.ty, rhs_reg, region)?;
                        self.truncate_envs(env_mark);
                        return Ok(true);
                    }
                    record_rhs(&mut unique_rhs, rhs_reg);
                }
                // Also elaborate supertrait outlives bounds (`type Convert: ConvertTx<...>` where
                // `trait ConvertTx: 'static`).
                let mut visited = [ItemKey(0); MAX_ELAB_VISITED];
                let mut vlen = 0usize;
                for cv in views {
                    if cv.outlives.is_some() {
                        continue;
                    }
                    let c = Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env: aenv } };
                    if let Ok(PNode::ExTrait(ct, _)) = self.node(c)
                        && self.fits(c).is_ok()
                        && let Ok(c_own) = self.own(c)
                        && let Ok(ckids) = self.kids_of(c)
                        && ckids.len() > 0
                        && let Some((_, crest)) = c.slots.split(c_own)
                        && let Ok((c_self, _)) = self.kid(c, &ckids, crest, 0)
                        && self.erased_sides(c_self, cur)
                    {
                        let c = self.instantiate_clause_side(c, cv.binders)?;
                        let (_, crest) = c.slots.split(c_own).ok_or(self.malformed(c.ty, Bad::StepBound))?;
                        let (c_self, _) = self.kid(c, &ckids, crest, 0)?;
                        if self.elaborate_outlives_from_trait(ct, c_self, c, true, cur, region, &mut unique_rhs, &mut visited, &mut vlen)? {
                            self.truncate_envs(env_mark);
                            return Ok(true);
                        }
                    }
                }
                self.truncate_envs(env_mark);
            }
        }
        let owner: &'c dyn Owner = self.owner;
        let clauses = owner.clauses();
        let sig = owner.sig();
        if !clauses.is_empty() || sig.is_some() {
            if let Some(oenv) = self.envs.owner_env() {
                for cv in clauses {
                    let Some(rhs) = cv.outlives else { continue };
                    if cv.binders != 0 {
                        continue;
                    }
                    let rhs_reg = if rhs == Region::Static { Reg::Static } else { Reg::Owner(rhs) };
                    let c = Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env: oenv } };
                    if !self.alias_side_eq(c, cur) {
                        continue;
                    }
                    if rhs_reg == Reg::Static || rhs_reg == region {
                        self.outlives(cur.ty, rhs_reg, region)?;
                        return Ok(true);
                    }
                    record_rhs(&mut unique_rhs, rhs_reg);
                }
                if let Some(sig) = sig {
                    // Walk the caller's signature (`FnPtr([inputs.., output])`) to collect RFC 1214
                    // implied bounds `&'rhs T` -> `T: 'rhs` where `T` contains an unselected alias.
                    let mut stack: [Option<(Side<'c>, Option<Reg>)>; 64] = [None; 64];
                    let mut sp = 1usize;
                    if let Some(slot) = stack.get_mut(0) {
                        *slot = Some((Side { ty: sig.ty, slots: Slots::Sig { col: sig.col, env: oenv } }, None));
                    }
                    let mut steps = 0u32;
                    while let Some(top_idx) = sp.checked_sub(1) {
                        sp = top_idx;
                        steps += 1;
                        if steps > 256 {
                            break;
                        }
                        let Some((s, under)) = stack.get(top_idx).copied().flatten() else { continue };
                        let Ok(n) = self.node(s) else { continue };
                        if matches!(n, PNode::Alias { .. }) {
                            if let Some(rhs_reg) = under
                                && self.alias_side_eq(s, cur)
                            {
                                if rhs_reg == Reg::Static || rhs_reg == region {
                                    self.outlives(cur.ty, rhs_reg, region)?;
                                    return Ok(true);
                                }
                                record_rhs(&mut unique_rhs, rhs_reg);
                            }
                            continue;
                        }
                        let descend = matches!(
                            n,
                            PNode::FnPtr(..)
                                | PNode::Ref(..)
                                | PNode::Ptr(..)
                                | PNode::Slice(_)
                                | PNode::Array(..)
                                | PNode::Pat(..)
                                | PNode::Tuple(_)
                                | PNode::Record(..)
                                | PNode::Union(_)
                                | PNode::Adt(..)
                        );
                        if !descend || self.fits(s).is_err() {
                            continue;
                        }
                        let next_under = if matches!(n, PNode::Ref(..)) {
                            match self.slot(s, 0) {
                                Ok(RegAt::Known(r @ (Reg::Static | Reg::Owner(_)))) => Some(r),
                                _ => None,
                            }
                        } else {
                            under
                        };
                        let (Ok(own), Ok(kids)) = (self.own(s), self.kids_of(s)) else { continue };
                        let Some((_, mut rest)) = s.slots.split(own) else { continue };
                        for k in 0..kids.len() {
                            let Ok((child, rem)) = self.kid(s, &kids, rest, k) else { break };
                            rest = rem;
                            if let Some(slot) = stack.get_mut(sp) {
                                *slot = Some((child, next_under));
                                sp += 1;
                            }
                        }
                    }
                }
            }
        }
        if matches!(region, Reg::Origin(_) | Reg::Slot(_))
            && let Some(Some(rhs_reg)) = unique_rhs
        {
            self.outlives(cur.ty, rhs_reg, region)?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Elaborates transitive supertrait outlives bounds (`Self: 'rhs`) starting from a proven
    /// trait bound `seed_self: seed_tr` for subject `cur`. Returns `Ok(true)` if an outlives bound
    /// immediately proves `cur: region` (`'static` or `== region`), and records other bounds into
    /// `unique_rhs`.
    #[allow(clippy::too_many_arguments)]
    fn elaborate_outlives_from_trait(
        &mut self,
        seed_tr: ItemKey,
        seed_self: Side<'c>,
        seed_trait: Side<'c>,
        seed_has_self: bool,
        cur: Side<'c>,
        region: Reg,
        unique_rhs: &mut Option<Option<Reg>>,
        visited: &mut [ItemKey; MAX_ELAB_VISITED],
        vlen: &mut usize,
    ) -> Result<bool, Refused> {
        let mut stack: [Option<(ItemKey, Side<'c>, Side<'c>, bool, u32)>; MAX_ELAB_STACK] = [None; MAX_ELAB_STACK];
        let mut sp = 0usize;
        if let Some(slot) = stack.get_mut(sp) {
            *slot = Some((seed_tr, seed_self, seed_trait, seed_has_self, 0));
            sp += 1;
        }
        while sp > 0 {
            sp -= 1;
            let Some((cur_tr, cur_self, cur_trait, cur_has_self, cur_depth)) = stack.get_mut(sp).and_then(Option::take) else {
                continue;
            };
            if cur_depth >= MAX_ELAB_DEPTH || *vlen >= MAX_ELAB_VISITED || visited.iter().take(*vlen).any(|&k| k == cur_tr) {
                continue;
            }
            if let Some(slot) = visited.get_mut(*vlen) {
                *slot = cur_tr;
                *vlen += 1;
            } else {
                continue;
            }
            let Some((tenv, views)) = self.trait_env_from_seed(cur_tr, cur_self, cur_trait, cur_has_self)? else {
                continue;
            };
            for cv in views {
                if let Some(rhs) = cv.outlives {
                    if cv.binders != 0 {
                        continue;
                    }
                    let tc = Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env: tenv } };
                    let matches_self = matches!(self.node(tc), Ok(PNode::Param(0)))
                        || matches!(self.resolve(tc), Ok(Resolved::Side(res)) if self.alias_side_eq(res, cur));
                    if !matches_self {
                        continue;
                    }
                    let Ok(RegAt::Known(rhs_reg)) = self.envs.reg(tenv, rhs) else { continue };
                    if rhs_reg == Reg::Static || rhs_reg == region {
                        self.outlives(cur.ty, rhs_reg, region)?;
                        return Ok(true);
                    }
                    match *unique_rhs {
                        None => *unique_rhs = Some(Some(rhs_reg)),
                        Some(Some(prev)) if prev == rhs_reg => {}
                        _ => *unique_rhs = Some(None),
                    }
                } else if cur_depth + 1 < MAX_ELAB_DEPTH {
                    let tc = Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env: tenv } };
                    if let Ok(PNode::ExTrait(super_ct, _)) = self.node(tc)
                        && self.fits(tc).is_ok()
                        && let Ok(c_own) = self.own(tc)
                        && let Ok(ckids) = self.kids_of(tc)
                        && ckids.len() > 0
                        && let Some((_, crest)) = tc.slots.split(c_own)
                        && let Ok((c_self, _)) = self.kid(tc, &ckids, crest, 0)
                        && self.erased_sides(c_self, cur)
                    {
                        let tc = self.instantiate_clause_side(tc, cv.binders)?;
                        let (_, crest) = tc.slots.split(c_own).ok_or(self.malformed(tc.ty, Bad::StepBound))?;
                        let (c_self, _) = self.kid(tc, &ckids, crest, 0)?;
                        if let Some(slot) = stack.get_mut(sp) {
                            *slot = Some((super_ct, c_self, tc, true, cur_depth + 1));
                            sp += 1;
                        }
                    }
                }
            }
        }
        Ok(false)
    }

    /// Whether `cur` (`PNode::Opaque(op_key, ..)`) is already proven to outlive `region` by its
    /// own opaque item bounds (`+ 'static`, `+ 'r`) or any supertrait of its opaque trait bounds
    /// (e.g. `impl ExecutableTxIterator<Self>` where `ExecutableTxTuple: Send + 'static`).
    fn opaque_assumed_outlives(&mut self, op_key: ItemKey, cur: Side<'c>, region: Reg) -> Result<bool, Refused> {
        let selected: &'c dyn Selected = self.selected;
        let Some((op_lts, views)) = selected.opaque_clauses(op_key) else {
            return Ok(false);
        };
        if views.is_empty() || self.fits(cur).is_err() {
            return Ok(false);
        }
        let mut unique_rhs: Option<Option<Reg>> = None;
        let sub_kids = self.kids_of(cur)?;
        for cv in views {
            let Some(rhs) = cv.outlives else { continue };
            if cv.binders != 0 {
                continue;
            }
            if !matches!(self.sig_types.node(cv.col.ty()), Ok(PNode::Opaque(k, _)) if k == op_key) {
                continue;
            }
            let env_mark = self.envs.len();
            let Some(oenv) = self.envs.impl_env(op_lts, sub_kids.len()) else { continue };
            let c = Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env: oenv } };
            if self.fits(c).is_err() || self.walk(c, cur, Variance::In, 1).is_err() {
                self.truncate_envs(env_mark);
                continue;
            }
            for i in 0..op_lts {
                if let Ok(RegAt::Unbound { param, .. }) = self.envs.reg(oenv, Region::Param(i)) {
                    let o = self.out.fresh(Kind::Existential);
                    let _ = self.envs.bind_reg(oenv, param, Reg::Origin(o));
                }
            }
            let Ok(RegAt::Known(rhs_reg)) = self.envs.reg(oenv, rhs) else {
                self.truncate_envs(env_mark);
                continue;
            };
            self.truncate_envs(env_mark);
            if rhs_reg == Reg::Static || rhs_reg == region {
                self.outlives(cur.ty, rhs_reg, region)?;
                return Ok(true);
            }
            match unique_rhs {
                None => unique_rhs = Some(Some(rhs_reg)),
                Some(Some(prev)) if prev == rhs_reg => {}
                _ => unique_rhs = Some(None),
            }
        }
        let mut visited = [ItemKey(0); MAX_ELAB_VISITED];
        let mut vlen = 0usize;
        for cv in views {
            if cv.outlives.is_some() {
                continue;
            }
            let Ok(PNode::ExTrait(ct, _)) = self.sig_types.node(cv.col.ty()) else { continue };
            let env_mark = self.envs.len();
            let Some(c) = self.opaque_clause_side(cv, op_lts, cur, op_key)? else { continue };
            let Ok(c_own) = self.own(c) else {
                self.truncate_envs(env_mark);
                continue;
            };
            let Ok(ckids) = self.kids_of(c) else {
                self.truncate_envs(env_mark);
                continue;
            };
            if ckids.len() == 0 {
                self.truncate_envs(env_mark);
                continue;
            }
            let Some((_, crest)) = c.slots.split(c_own) else {
                self.truncate_envs(env_mark);
                continue;
            };
            let Ok((c_self, _)) = self.kid(c, &ckids, crest, 0) else {
                self.truncate_envs(env_mark);
                continue;
            };
            if !self.erased_sides(c_self, cur) {
                self.truncate_envs(env_mark);
                continue;
            }
            let hit = self.elaborate_outlives_from_trait(ct, c_self, c, true, cur, region, &mut unique_rhs, &mut visited, &mut vlen)?;
            self.truncate_envs(env_mark);
            if hit {
                return Ok(true);
            }
        }
        if matches!(region, Reg::Origin(_) | Reg::Slot(_))
            && let Some(Some(rhs_reg)) = unique_rhs
        {
            self.outlives(cur.ty, rhs_reg, region)?;
            return Ok(true);
        }
        Ok(false)
    }

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
                        match selected.select_keyed(Ask { trait_, name: Some(name), written: cur.ty }, &key) {
                            Selection::Impl { value: Some(_), .. } => {
                                let value = self.normalize(cur, n, d)?;
                                enter = Some((value, d + 1));
                                continue;
                            }
                            Selection::Not(_) => {
                                // Caller's own `where <Alias>: 'rhs` clause or RFC 1214 implied
                                // bound (`&'rhs <Alias>` in the caller's signature): if the
                                // caller already assumes `cur: 'rhs` and `'rhs` can prove
                                // `region`, no further outlives requirement on `cur`'s arguments
                                // is needed.
                                if self.alias_assumed_outlives(cur, region)? {
                                    continue;
                                }
                                // Builtin `<async { .. } as Future>::Output`: when the future's
                                // output type carries zero lifetime slots (e.g. `()` or a bare
                                // type parameter `T`), normalize straight to `output` so its
                                // outlives bound does not constrain the future's own captures.
                                let kids = self.kids_of(cur)?;
                                if kids.len() == 1
                                    && let Ok(own) = self.own(cur)
                                    && let Some((_, rest)) = cur.slots.split(own)
                                    && let Ok((subj, _)) = self.kid(cur, &kids, rest, 0)
                                    && let Ok(Resolved::Side(subj)) = self.resolve(subj)
                                    && let Ok(PNode::Future(_, output, _)) = self.node(subj)
                                {
                                    let out_side = Side { ty: output, slots: Slots::Caller(crate::SlotRange::EMPTY) };
                                    if self.fits(out_side).is_ok() {
                                        enter = Some((out_side, d + 1));
                                        continue;
                                    }
                                }
                            }
                            _ => {}
                        }
                        // Not selected: the alias outlives `'r` if its arguments do (RFC 1214's
                        // projection rule, its sufficient half).
                    }
                    PNode::Opaque(op_key, _) => {
                        if self.opaque_assumed_outlives(op_key, cur, region)? {
                            continue;
                        }
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
        let raw_subject = match self.resolve(subject)? {
            Resolved::Side(s) => s,
            Resolved::Unbound { param, .. } => return Err(self.malformed(subject.ty, Bad::Unconstrained { index: param })),
        };
        if self.try_prove_trait_non_impl(raw_subject, trait_ref, false, trait_)? {
            return Ok(());
        }
        let (subject, normalized) = self.try_normalize_subject_flag(raw_subject, 0);
        if (normalized || subject.ty != raw_subject.ty) && self.try_prove_trait_non_impl(subject, trait_ref, false, trait_)? {
            return Ok(());
        }
        let param_miss = matches!(self.node(subject)?, PNode::Param(_));
        let mut norm_args = [None; MAX_NORM_ARGS];
        if self.fits(trait_ref).is_ok()
            && let Ok(own) = self.own(trait_ref)
            && let Ok(kids) = self.kids_of(trait_ref)
            && let Some((_, mut rem)) = trait_ref.slots.split(own)
        {
            for k in 0..kids.len() {
                if let Ok((arg, r)) = self.kid(trait_ref, &kids, rem, k) {
                    rem = r;
                    if let Some(slot) = norm_args.get_mut((k + 1) as usize)
                        && let Ok(Resolved::Side(arg_s)) = self.resolve(arg)
                    {
                        let (n, changed) = self.try_normalize_subject_flag(arg_s, 0);
                        if changed {
                            *slot = Some(n);
                        }
                    }
                } else {
                    break;
                }
            }
        }
        let selected: &'c dyn Selected = self.selected;
        let key = Keyer { walk: &*self, at: AskAt::Subject { subject, of: trait_ref, norm: norm_args }, budget: Cell::new(MOST_WALK_STEPS) };
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
                let (_, rest) = trait_ref.slots.split(own).ok_or(self.malformed(trait_ref.ty, Bad::StepBound))?;
                self.walk_header_kids(header, &hkids, hrest, trait_ref, &kids, rest, Some(subject), false, 1)?;
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
    fn truncate_envs(&mut self, mark: usize) {
        self.envs.truncate(mark);
        self.impl_seen.retain(|&(_, e)| e.index() < mark);
    }

    /// Region-preserving structural equality between two resolved sides (no normalization).
    /// Conservative: returns `false` on any unbound parameter, budget exhaustion, or error.
    fn exact_side_eq(&self, a: Side<'c>, b: Side<'c>) -> bool {
        const MAX_FRAMES: usize = 64;
        #[derive(Clone, Copy)]
        struct EqFrame<'c> {
            a: Side<'c>,
            b: Side<'c>,
            ra: Slots<'c>,
            rb: Slots<'c>,
            next_k: u32,
            depth: u32,
        }
        let mut stack: [Option<EqFrame<'c>>; MAX_FRAMES] = [None; MAX_FRAMES];
        let mut sp = 0usize;
        let mut budget = 256u32;
        let mut enter = Some((a, b, 0u32));
        loop {
            if let Some((a, b, d)) = enter.take() {
                if d >= 64 || budget == 0 {
                    return false;
                }
                budget -= 1;
                let (Ok(Resolved::Side(a)), Ok(Resolved::Side(b))) = (self.resolve(a), self.resolve(b)) else {
                    return false;
                };
                if a.ty == b.ty {
                    let same_slots = match (a.slots, b.slots) {
                        (Slots::Caller(ra), Slots::Caller(rb)) => ra == rb,
                        (Slots::Sig { col: ca, env: ea }, Slots::Sig { col: cb, env: eb })
                        | (Slots::CallerEnv { col: ca, env: ea }, Slots::CallerEnv { col: cb, env: eb }) => {
                            ea == eb && core::ptr::eq(ca.as_ptr(), cb.as_ptr()) && ca.len() == cb.len()
                        }
                        (
                            Slots::CallerFresh { env: ea, start: sa, len: la },
                            Slots::CallerFresh { env: eb, start: sb, len: lb },
                        ) => ea == eb && sa == sb && la == lb,
                        _ => false,
                    };
                    if same_slots {
                        continue;
                    }
                }
                let (Ok(na), Ok(nb)) = (self.node(a), self.node(b)) else {
                    return false;
                };
                if is_const(na) && is_const(nb) {
                    continue;
                }
                if !head_eq(na, nb) {
                    return false;
                }
                let (Ok(oa), Ok(ob)) = (self.own(a), self.own(b)) else {
                    return false;
                };
                if oa != ob {
                    return false;
                }
                for k in 0..oa.get() {
                    let (Ok(RegAt::Known(ra)), Ok(RegAt::Known(rb))) = (self.slot(a, k), self.slot(b, k)) else {
                        return false;
                    };
                    if ra != rb {
                        return false;
                    }
                }
                let (Ok(ka), Ok(kb)) = (self.kids_of(a), self.kids_of(b)) else {
                    return false;
                };
                if ka.len() != kb.len() {
                    return false;
                }
                if ka.len() > 0 {
                    let (Some((_, ra)), Some((_, rb))) = (a.slots.split(oa), b.slots.split(ob)) else {
                        return false;
                    };
                    let Some(slot) = stack.get_mut(sp) else {
                        return false;
                    };
                    *slot = Some(EqFrame { a, b, ra, rb, next_k: 0, depth: d });
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
                let (Ok((ca, nra)), Ok((cb, nrb))) = (self.kid(frame.a, &ka, frame.ra, k), self.kid(frame.b, &kb, frame.rb, k)) else {
                    return false;
                };
                frame.ra = nra;
                frame.rb = nrb;
                frame.next_k = k + 1;
                if let Some(slot) = stack.get_mut(top_idx) {
                    *slot = Some(frame);
                }
                enter = Some((ca, cb, frame.depth + 1));
            } else {
                sp = top_idx;
            }
        }
    }

    fn impl_env_eq(&self, a: EnvId, b: EnvId) -> bool {
        let (Some(ra), Some(rb)) = (self.envs.env_regs(a), self.envs.env_regs(b)) else {
            return false;
        };
        if ra != rb {
            return false;
        }
        let (Some(ta), Some(tb)) = (self.envs.env_types(a), self.envs.env_types(b)) else {
            return false;
        };
        if ta.len() != tb.len() {
            return false;
        }
        ta.iter().zip(tb.iter()).all(|(xa, xb)| match (*xa, *xb) {
            (Some(sa), Some(sb)) => self.exact_side_eq(sa, sb),
            (None, None) => true,
            _ => false,
        })
    }

    fn impl_where(&mut self, imp: ItemKey, trait_: ItemKey, node: PTy, env: EnvId, lifetimes: u32) -> Result<(), Refused> {
        let selected: &'c dyn Selected = self.selected;
        let w = selected.impl_where(imp).map_err(|why| Refused::Unselected { item: self.item, node, trait_, name: None, why })?;
        if w.params.is_empty() && w.outlives.is_empty() && w.views.is_empty() {
            return Ok(());
        }
        // Speculative erased-equality normalization only reads the impl's associated type head;
        // when all impl type parameters are already bound by the header walk, running `impl_where`
        // cannot bind any type parameter and would emit redundant speculative work/constraints.
        if self.in_erased_norm && self.envs.all_types_bound(env) {
            return Ok(());
        }
        let cacheable = self.envs.all_impl_bound(env, lifetimes);
        if cacheable && self.impl_seen.iter().any(|&(k, prev)| k == imp && self.impl_env_eq(prev, env)) {
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
        let seen_mark = self.impl_seen.len();
        if cacheable && self.impl_seen.len() < MOST_IMPL_SEEN {
            self.impl_seen.push((imp, env));
        }
        let saved = (self.bound, self.bound_len, self.hr, self.hr_len, self.binder_v);
        self.nested += 1;
        let res = self.impl_where_in(w, trait_, node, env);
        self.nested -= 1;
        (self.bound, self.bound_len, self.hr, self.hr_len, self.binder_v) = saved;
        if res.is_err() {
            self.impl_seen.truncate(seen_mark);
        }
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
                self.proj_of(side, cv.binders)?;
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
                self.tie_of(side, cv.binders)?;
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
    /// Like [`Walk::inert`], but normalizes nested `PNode::Alias` projections (e.g.
    /// `Recovered<<<Evm::BlockExecutorFactory as BlockExecutorFactory>::EvmTypes as
    /// EvmTypesHost>::Tx>` -> `Recovered<TempoTxEnv>`) while checking whether a type carries zero
    /// lifetimes and zero caller type parameters.
    fn inert_norm(&mut self, s: Side<'c>, depth: u32, budget: &mut u32) -> Result<bool, Refused> {
        let mut fast_b = *budget;
        if self.inert(s, depth, &mut fast_b)? {
            *budget = fast_b;
            return Ok(true);
        }
        if self.fits(s)? != SlotCount::ZERO {
            return Ok(false);
        }
        const MAX_FRAMES: usize = 257;
        #[derive(Clone, Copy)]
        struct InertFrame<'c> {
            s: Side<'c>,
            rest: Slots<'c>,
            next_k: u32,
            depth: u32,
        }
        let env_mark = self.envs.len();
        let saved_budget = self.budget;
        let seen_mark = self.impl_seen.len();
        let mut stack: [Option<InertFrame<'c>>; MAX_FRAMES] = [None; MAX_FRAMES];
        let mut sp: usize = 0;
        let mut enter = Some((s, depth));
        let res = loop {
            if let Some((cur, d)) = enter.take() {
                if d > MOST_WALK_DEPTH || *budget == 0 {
                    break Ok(false);
                }
                *budget -= 1;
                let cur = match self.resolve(cur) {
                    Ok(Resolved::Side(s)) => s,
                    Ok(Resolved::Unbound { .. }) => break Ok(false),
                    Err(e) => break Err(e),
                };
                match self.fits(cur) {
                    Ok(n) if n == SlotCount::ZERO => {}
                    Ok(_) => break Ok(false),
                    Err(e) => break Err(e),
                }
                let node = match self.node(cur) {
                    Ok(n) => n,
                    Err(e) => break Err(e),
                };
                let cur = if matches!(node, PNode::Alias { .. }) {
                    let mut kb = *budget;
                    if self.inert(cur, d, &mut kb).unwrap_or(false) {
                        *budget = kb;
                        continue;
                    }
                    self.try_normalize_subject(cur, d)
                } else {
                    cur
                };
                match self.fits(cur) {
                    Ok(n) if n == SlotCount::ZERO => {}
                    Ok(_) => break Ok(false),
                    Err(e) => break Err(e),
                }
                match self.node(cur) {
                    Ok(PNode::Param(_) | PNode::Infer | PNode::Error) => break Ok(false),
                    Ok(PNode::Closure { .. } | PNode::Future(..) | PNode::Coroutine { .. }) => {
                        let Slots::Caller(_) = cur.slots else { break Ok(false) };
                        let owner: &'c dyn Owner = self.owner;
                        if !matches!(owner.body_params(cur.ty), Some(p) if p.is_empty()) {
                            break Ok(false);
                        }
                        continue;
                    }
                    Ok(_) => {}
                    Err(e) => break Err(e),
                }
                let kids = match self.kids_of(cur) {
                    Ok(k) => k,
                    Err(e) => break Err(e),
                };
                if kids.len() > 0 {
                    let Some(slot) = stack.get_mut(sp) else {
                        break Ok(false);
                    };
                    *slot = Some(InertFrame { s: cur, rest: cur.slots, next_k: 0, depth: d });
                    sp += 1;
                }
                continue;
            }
            let Some(top_idx) = sp.checked_sub(1) else {
                break Ok(true);
            };
            let Some(Some(mut frame)) = stack.get(top_idx).copied() else {
                break Ok(false);
            };
            let kids = match self.kids_of(frame.s) {
                Ok(k) => k,
                Err(e) => break Err(e),
            };
            if frame.next_k < kids.len() {
                let k = frame.next_k;
                let (c, r) = match self.kid(frame.s, &kids, frame.rest, k) {
                    Ok(cr) => cr,
                    Err(e) => break Err(e),
                };
                frame.rest = r;
                frame.next_k = k + 1;
                if let Some(slot) = stack.get_mut(top_idx) {
                    *slot = Some(frame);
                }
                enter = Some((c, frame.depth + 1));
            } else {
                sp = top_idx;
            }
        };
        self.impl_seen.truncate(seen_mark);
        self.truncate_envs(env_mark);
        self.budget = saved_budget.saturating_sub(1);
        res
    }

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
    /// Whether `s` (`ExTrait` or `ExProj`) has a subject that peels references down to a
    /// `PNode::Dynamic` trait object (`&dyn for<'a> Fn(..)`, `dyn for<'a> Tr<'a>`).
    fn is_dyn_subject(&self, s: Side<'c>) -> bool {
        let Ok(own) = self.own(s) else { return false };
        let Ok(kids) = self.kids_of(s) else { return false };
        if kids.len() == 0 {
            return false;
        }
        let Some((_, rest)) = s.slots.split(own) else { return false };
        let Ok((mut cur, _)) = self.kid(s, &kids, rest, 0) else { return false };
        for _ in 0..8 {
            let Ok(Resolved::Side(res)) = self.resolve(cur) else { return false };
            cur = res;
            match self.node(cur) {
                Ok(PNode::Dynamic(_)) => return true,
                Ok(PNode::Ref(..)) => {
                    let Ok(co) = self.own(cur) else { return false };
                    let Ok(ck) = self.kids_of(cur) else { return false };
                    let Some((_, cr)) = cur.slots.split(co) else { return false };
                    let Ok((inner, _)) = self.kid(cur, &ck, cr, 0) else { return false };
                    cur = inner;
                }
                _ => return false,
            }
        }
        false
    }

    /// Skolemizes a clause's own `for<..>` binder (`binder: 0`, `0..binders`) to universal
    /// placeholder origins so higher-ranked trait and projection goals (e.g.
    /// `for<'a> <Self::SolType as SolType>::Token<'a>: TokenSeq<'a>`) normalize and tie through
    /// impls and where-clauses soundly without leaving raw `Reg::Bound { binder: 0, .. }`
    /// outside a binder context. When the subject is a `dyn` object (`&dyn Fn(..)`), reuses
    /// canonical `dyn_placeholder(var)` ordinals so passing the same `&dyn` to multiple calls
    /// does not create conflicting placeholders across calls.
    fn skolemize_side(&mut self, s: Side<'c>, binders: u32) -> Result<Side<'c>, Refused> {
        if binders == 0 {
            return Ok(s);
        }
        let Slots::Sig { col, env } = s.slots else {
            return Ok(s);
        };
        if !col.iter().any(|r| matches!(r, Region::Late { binder: 0, .. })) {
            return Ok(s);
        }
        let use_dyn_ph = self.is_dyn_subject(s);
        let mut ord = 0u32;
        let out = &mut *self.out;
        let new_env = self
            .envs
            .skolemize_clause(env, binders, || {
                let o = if use_dyn_ph { out.dyn_placeholder(ord) } else { out.fresh(Kind::Placeholder) };
                ord += 1;
                o
            })
            .ok_or(self.malformed(s.ty, Bad::StepBound))?;
        Ok(Side { ty: s.ty, slots: Slots::Sig { col, env: new_env } })
    }

    /// Instantiates an assumed higher-ranked clause's own `for<..>` binder (`binder: 0`,
    /// `0..binders`) to fresh existential origins so matching its LHS against a goal binds those
    /// origins and returning its RHS (`ExProj` term) or seeding its supertraits leaves no raw
    /// `Reg::Bound { binder: 0, .. }` outside a binder context.
    fn instantiate_clause_side(&mut self, s: Side<'c>, binders: u32) -> Result<Side<'c>, Refused> {
        if binders == 0 {
            return Ok(s);
        }
        let Slots::Sig { col, env } = s.slots else {
            return Ok(s);
        };
        if !col.iter().any(|r| matches!(r, Region::Late { binder: 0, .. })) {
            return Ok(s);
        }
        let out = &mut *self.out;
        let new_env = self
            .envs
            .skolemize_clause(env, binders, || out.fresh(Kind::Existential))
            .ok_or(self.malformed(s.ty, Bad::StepBound))?;
        Ok(Side { ty: s.ty, slots: Slots::Sig { col, env: new_env } })
    }

    fn proj_of(&mut self, proj: Side<'c>, binders: u32) -> Result<(), Refused> {
        let orig_env = Self::env_of(proj);
        let proj = self.skolemize_side(proj, binders)?;
        let skolem_env = Self::env_of(proj);
        let res = self.proj_of_in(proj);
        if res.is_ok()
            && let (Some(from), Some(to)) = (skolem_env, orig_env)
        {
            self.envs.propagate_bindings(from, to);
        }
        res
    }

    fn proj_of_in(&mut self, proj: Side<'c>) -> Result<(), Refused> {
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
        if self.selected.is_zero_slot_builtin_assoc(trait_, name) {
            if self.fits(term)? == SlotCount::ZERO {
                return Ok(());
            }
            if name == blaze_typeck::Name::of("Metadata")
                && let Resolved::Side(subj_res) = self.resolve(subject)?
                && matches!(self.node(subj_res)?, PNode::Dynamic(_))
                && let Resolved::Side(term_res) = self.resolve(term)?
                && matches!(self.node(term_res)?, PNode::Adt(..))
                && self.own(term_res)? == SlotCount::ZERO
                && let Ok(tkids) = self.kids_of(term_res)
                && tkids.len() == 1
                && let Ok((dyn_arg, _)) = self.kid(term_res, &tkids, term_res.slots, 0)
                && let Resolved::Side(dyn_arg) = self.resolve(dyn_arg)?
                && matches!(self.node(dyn_arg)?, PNode::Dynamic(_))
            {
                return self.walk(subj_res, dyn_arg, Variance::In, 1);
            }
        }
        let raw_subject = match self.resolve(subject)? {
            Resolved::Side(s) => s,
            Resolved::Unbound { param, .. } => return Err(self.malformed(subject.ty, Bad::Unconstrained { index: param })),
        };
        let (subject, sub_changed) = self.try_normalize_subject_flag(raw_subject, 0);
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
        if (sub_changed || subject.ty != raw_subject.ty)
            && let Some(norm) = self.try_normalize_non_impl(proj, last, raw_subject, trait_, name)?
        {
            return self.walk(norm, term, Variance::In, 1);
        }
        // Reflexive projection equality (`<T as Tr<A..>>::N == <T as Tr<A..>>::N`, e.g.
        // `P: WriterIn<Buffer = B>` instantiated with `B = P::Buffer`, or
        // `Pool::Transaction::Pooled == <BasicNetworkPrimitives<_, Pool::Transaction::Pooled> as NetworkPrimitives>::PooledTransaction`):
        // normalize `term` first so projections that reduce to the same alias match reflexively,
        // then relate own lifetime slots and `[Self, A..]` invariantly, just as `walk_loop`'s
        // `same_alias` does.
        let term = if let Resolved::Side(term_res) = self.resolve(term)? {
            self.try_normalize_subject(term_res, 0)
        } else {
            term
        };
        if let Resolved::Side(term_res) = self.resolve(term)?
            && let PNode::Alias { trait_: t2, name: n2, .. } = self.node(term_res)?
            && t2 == trait_
            && n2 == name
            && self.fits(term_res).is_ok()
        {
            let t_own = self.own(term_res)?;
            let tkids = self.kids_of(term_res)?;
            if t_own == own && tkids.len() == last {
                let mut same_args = true;
                for k in 0..last {
                    let p_arg = if k == 0 { Some(subject) } else { self.erased_kid(proj, k) };
                    let t_arg = self.erased_kid(term_res, k);
                    if !matches!((p_arg, t_arg), (Some(pa), Some(ta)) if self.erased_sides(pa, ta)) {
                        same_args = false;
                        break;
                    }
                }
                if same_args {
                    for k in 0..own.get() {
                        let (x, y) = (self.slot(proj, k)?, self.slot(term_res, k)?);
                        self.pair(proj.ty, x, y, Variance::In, Self::env_of(proj), Self::env_of(term_res))?;
                    }
                    let (_, mut prest) = proj.slots.split(own).ok_or(self.malformed(proj.ty, Bad::StepBound))?;
                    let (_, mut trest) = term_res.slots.split(t_own).ok_or(self.malformed(term_res.ty, Bad::StepBound))?;
                    for k in 0..last {
                        let (p_kid, pr) = self.kid(proj, &kids, prest, k)?;
                        let (t_kid, tr) = self.kid(term_res, &tkids, trest, k)?;
                        prest = pr;
                        trest = tr;
                        let p_side = if k == 0 { subject } else { p_kid };
                        self.walk(p_side, t_kid, Variance::In, 1)?;
                    }
                    return Ok(());
                }
            }
        }
        let mut norm_args = [None; MAX_NORM_ARGS];
        if last > 1 {
            let (_, mut rem) = self.kid(proj, &kids, args_rest, 0)?;
            for k in 1..last {
                let (arg, r) = self.kid(proj, &kids, rem, k)?;
                rem = r;
                if let Some(slot) = norm_args.get_mut(k as usize)
                    && let Resolved::Side(arg_s) = self.resolve(arg)?
                {
                    let (n, changed) = self.try_normalize_subject_flag(arg_s, 0);
                    if changed {
                        *slot = Some(n);
                    }
                }
            }
        }
        let selected: &'c dyn Selected = self.selected;
        let key = Keyer { walk: &*self, at: AskAt::SubjectKids { subject, of: proj, len: last, norm: norm_args }, budget: Cell::new(MOST_WALK_STEPS) };
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
        let (_, hrest) = header.slots.split(tl).ok_or(self.malformed(header.ty, Bad::StepBound))?;
        self.walk_header_kids(header, &hkids, hrest, proj, &kids, args_rest, None, true, 1)?;
        self.impl_where(head.imp, trait_, proj.ty, env, head.lifetimes)?;
        let norm = Side { ty: value.ty, slots: Slots::Sig { col: value.col, env } };
        self.walk(norm, term, Variance::In, 1)
    }

    /// `T: Trait<A..>` laid subject-first (`ExTrait(Trait, [T, A..])`, a clause column's form):
    /// as [`Walk::tie`], with the subject read off the node.
    fn tie_of(&mut self, trait_ref: Side<'c>, binders: u32) -> Result<(), Refused> {
        let orig_env = Self::env_of(trait_ref);
        let trait_ref = self.skolemize_side(trait_ref, binders)?;
        let skolem_env = Self::env_of(trait_ref);
        let res = self.tie_of_in(trait_ref);
        if res.is_ok()
            && let (Some(from), Some(to)) = (skolem_env, orig_env)
        {
            self.envs.propagate_bindings(from, to);
        }
        res
    }

    fn tie_of_in(&mut self, trait_ref: Side<'c>) -> Result<(), Refused> {
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
        let (_, args_rest) = trait_ref.slots.split(own).ok_or(self.malformed(trait_ref.ty, Bad::StepBound))?;
        let (subject, rest) = self.kid(trait_ref, &kids, args_rest, 0)?;
        let raw_subject = match self.resolve(subject)? {
            Resolved::Side(s) => s,
            Resolved::Unbound { param, .. } => return Err(self.malformed(subject.ty, Bad::Unconstrained { index: param })),
        };
        if self.try_prove_trait_non_impl(raw_subject, trait_ref, true, trait_)? {
            return Ok(());
        }
        let (subject, normalized) = self.try_normalize_subject_flag(raw_subject, 0);
        // Normalize any alias trait arguments (`k in 1..kids.len()`) so both the all-inert check
        // and `select_keyed` see concrete normalized types (e.g. `EthApiError: From<M::Err> +
        // From<H::Err>` distinguishes `From<ErrA>` from `From<ErrB>` instead of colliding as
        // `TwoSources`).
        let mut norm_args = [None; MAX_NORM_ARGS];
        let mut norm_ok = true;
        {
            let mut nrest = rest;
            for k in 1..kids.len() {
                let (arg, r) = self.kid(trait_ref, &kids, nrest, k)?;
                nrest = r;
                match self.resolve(arg)? {
                    Resolved::Side(s) => {
                        let (n, changed) = self.try_normalize_subject_flag(s, 0);
                        if changed && let Some(slot) = norm_args.get_mut(k as usize) {
                            *slot = Some(n);
                        }
                    }
                    Resolved::Unbound { .. } => norm_ok = false,
                }
            }
        }
        // If normalizing the subject or any alias trait argument yields an all-inert goal
        // (e.g. `usize: Sum<<Map<Iter<'a, _>, C> as Iterator>::Item>` -> `usize: Sum<usize>`),
        // the normalized impls' own where-clauses have already been checked and the goal itself
        // relates no region (so `selected_rows` rightly skipped filing a row for it).
        if own == SlotCount::ZERO && norm_ok {
            let mut b = MOST_WALK_STEPS;
            if self.inert_norm(subject, 0, &mut b)? {
                let mut all_inert = true;
                let mut irest = rest;
                for k in 1..kids.len() {
                    let (raw_arg, r) = self.kid(trait_ref, &kids, irest, k)?;
                    irest = r;
                    let arg = match norm_args.get(k as usize).copied().flatten() {
                        Some(n) => n,
                        None => match self.resolve(raw_arg)? {
                            Resolved::Side(s) => self.try_normalize_subject(s, 0),
                            Resolved::Unbound { .. } => {
                                all_inert = false;
                                break;
                            }
                        },
                    };
                    let mut kb = MOST_WALK_STEPS;
                    if !self.inert_norm(arg, 0, &mut kb)? {
                        all_inert = false;
                        break;
                    }
                }
                if all_inert {
                    return Ok(());
                }
            }
        }
        if (normalized || subject.ty != raw_subject.ty) && self.try_prove_trait_non_impl(subject, trait_ref, true, trait_)? {
            return Ok(());
        }
        let param_miss = matches!(self.node(subject)?, PNode::Param(_));
        let key = Keyer { walk: &*self, at: AskAt::SubjectKids { subject, of: trait_ref, len: kids.len(), norm: norm_args }, budget: Cell::new(MOST_WALK_STEPS) };
        let head = match selected.select_keyed(Ask { trait_, name: None, written: trait_ref.ty }, &key) {
            Selection::Impl { head, .. } => head,
            Selection::Not(NotSelected::Builtin) if self.nothing_to_tie(trait_ref, 1)? => return Ok(()),
            Selection::Not(NotSelected::GenericSelf(_)) if selected.is_auto_trait(trait_) && self.nothing_to_tie(trait_ref, 1)? => return Ok(()),
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
        self.walk_header_kids(header, &hkids, hrest, trait_ref, &kids, args_rest, Some(subject), true, 1)?;
        self.impl_where(head.imp, trait_, trait_ref.ty, env, head.lifetimes)
    }

    // ---- Non-impl proofs: builtin Fn*, owner bounds + supertraits, Dynamic, Opaque, Alias ----

    /// Builtin `Fn` / `FnMut` / `FnOnce` proof for a `FnPtr`, caller `Closure`, or `FnDef`
    /// (including through `&` / `&mut` reference wrappers per `core::ops::function`'s blanket
    /// impls for `&F` and `&mut F`). Relates `args_tuple` against the callable's inputs (and
    /// `want_output` against its output, when called from `proj_of`), returning the callable's
    /// output side on match.
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
        // Peel `&` / `&mut` layers (`impl FnOnce/FnMut/Fn for &F`, `impl FnOnce/FnMut for &mut F`
        // forward `Output = F::Output` with no extra lifetime constraint on the reference itself;
        // well-formedness of `&'r F` is enforced by `wf` and outlives by `components`).
        let mut subject = subject;
        for _ in 0..8 {
            let Resolved::Side(s) = self.resolve(subject)? else { break };
            subject = s;
            let PNode::Ref(_, _) = self.node(subject)? else { break };
            self.fits(subject)?;
            let own = self.own(subject)?;
            let kids = self.kids_of(subject)?;
            if kids.len() != 1 {
                break;
            }
            let (_, rest) = subject.slots.split(own).ok_or(self.malformed(subject.ty, Bad::StepBound))?;
            let (inner, _) = self.kid(subject, &kids, rest, 0)?;
            subject = inner;
        }
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
                let mut early = 0u32;
                for i in 0..params.count() {
                    let is_late = params.is_late(i).unwrap_or(true);
                    if !is_late && early < sub_own.get() {
                        match self.slot(subject, early)? {
                            RegAt::Known(r) => self.envs.bind_reg(fn_env, i, r).map_err(|w| self.malformed(node, w))?,
                            RegAt::Unbound { param, .. } => return Err(self.malformed(node, Bad::Unconstrained { index: param })),
                        }
                        early += 1;
                    } else {
                        let o = self.out.fresh(Kind::Existential);
                        self.envs.bind_reg(fn_env, i, Reg::Origin(o)).map_err(|w| self.malformed(node, w))?;
                    }
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
    #[allow(clippy::too_many_arguments)]
    /// Whether a clause's `Self` argument (`kid 0`) matches `subject` erased, allowing a
    /// top-level forwarding alias on `clause` (e.g. `<RpcNodeCoreFor<N> as RpcNodeCore>::Evm`
    /// normalized via caller's `where RpcNodeCoreFor<N>: RpcNodeCore<Evm = N::Evm>`) to match
    /// `subject` (`<N as FullNodeComponents>::Evm`) without relaxing `erased_sides_norm`'s `d > 0`
    /// guard for `walk_loop`'s `same_alias`.
    fn match_clause_self(&mut self, clause: Side<'c>, c_own: SlotCount, ckids: &Kids<'c>, subject: Side<'c>) -> bool {
        let Some(c_self) = self.erased_kid(clause, 0) else { return false };
        if self.erased_sides(c_self, subject) {
            return true;
        }
        if !self.in_erased_norm
            && let Some((_, crest)) = clause.slots.split(c_own)
            && let Ok((c0, _)) = self.kid(clause, ckids, crest, 0)
            && let Ok(Resolved::Side(c0)) = self.resolve(c0)
            && matches!(self.node(c0), Ok(PNode::Alias { .. }))
            && self.fits(c0).is_ok()
        {
            let env_mark = self.envs.len();
            let saved_budget = self.budget;
            let seen_mark = self.impl_seen.len();
            let (c_norm, changed) = self.try_normalize_subject_flag(c0, 0);
            let matched = changed && self.erased_sides(c_norm, subject);
            self.impl_seen.truncate(seen_mark);
            self.truncate_envs(env_mark);
            self.budget = saved_budget.saturating_sub(1);
            if matched {
                return true;
            }
        }
        false
    }

    fn match_trait_clause(&mut self, clause: Side<'c>, binders: u32, subject: Side<'c>, trait_ref: Side<'c>, has_subject: bool, allow_alias: bool) -> Result<bool, Refused> {
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
        if !self.match_clause_self(clause, c_own, &ckids, subject) {
            return Ok(false);
        }
        let w_off = u32::from(has_subject);
        if non_self > 0 {
            let Some((_, crest0)) = clause.slots.split(c_own) else { return Ok(false) };
            let Ok((_, mut crest)) = self.kid(clause, &ckids, crest0, 0) else { return Ok(false) };
            let Some((_, mut wrest)) = trait_ref.slots.split(w_own) else { return Ok(false) };
            if has_subject {
                let Ok((_, wr)) = self.kid(trait_ref, &wkids, wrest, 0) else { return Ok(false) };
                wrest = wr;
            }
            for k in 0..non_self {
                let (Ok((ca, cr)), Ok((wa, wr))) = (
                    self.kid(clause, &ckids, crest, k + 1),
                    self.kid(trait_ref, &wkids, wrest, k + w_off),
                ) else {
                    return Ok(false);
                };
                crest = cr;
                wrest = wr;
                if !self.match_clause_arg(ca, wa, allow_alias) {
                    return Ok(false);
                }
            }
        }
        let clause = self.instantiate_clause_side(clause, binders)?;
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
        if non_self > 0 {
            let Some((_, mut prest)) = pred.slots.split(p_own) else { return Ok(false) };
            let Some((_, mut wrest)) = trait_ref.slots.split(w_own) else { return Ok(false) };
            if has_subject {
                let Ok((_, wr)) = self.kid(trait_ref, &wkids, wrest, 0) else { return Ok(false) };
                wrest = wr;
            }
            for k in 0..non_self {
                let (Ok((pa, pr)), Ok((wa, wr))) = (
                    self.kid(pred, &pkids, prest, k),
                    self.kid(trait_ref, &wkids, wrest, k + w_off),
                ) else {
                    return Ok(false);
                };
                prest = pr;
                wrest = wr;
                if !self.match_clause_arg(pa, wa, allow_alias) {
                    return Ok(false);
                }
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
    /// `want` (an `Alias` or `ExProj` with `want_arg_count` arguments `[Self, A..]`) using its
    /// already-normalized `subject` for argument `0`, returning the clause's `term` side on match.
    #[allow(clippy::too_many_arguments)]
    fn match_proj_clause(
        &mut self,
        clause: Side<'c>,
        binders: u32,
        subject: Side<'c>,
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
        if want_arg_count > 0 {
            if !self.match_clause_self(clause, c_own, &ckids, subject) {
                return Ok(None);
            }
            if want_arg_count > 1 {
                let Some((_, crest0)) = clause.slots.split(c_own) else { return Ok(None) };
                let Ok((_, mut crest)) = self.kid(clause, &ckids, crest0, 0) else { return Ok(None) };
                let Some((_, wrest0)) = want.slots.split(w_own) else { return Ok(None) };
                let Ok((_, mut wrest)) = self.kid(want, &wkids, wrest0, 0) else { return Ok(None) };
                for k in 1..want_arg_count {
                    let (Ok((ca, cr)), Ok((wa, wr))) = (
                        self.kid(clause, &ckids, crest, k),
                        self.kid(want, &wkids, wrest, k),
                    ) else {
                        return Ok(None);
                    };
                    crest = cr;
                    wrest = wr;
                    if !self.match_clause_arg(ca, wa, allow_alias) {
                        return Ok(None);
                    }
                }
            }
        }
        let clause = self.instantiate_clause_side(clause, binders)?;
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
            let rhs = if k == 0 { subject } else { w };
            self.walk_with_binder(c, rhs, Variance::In, BinderCtx::Clause, 1)?;
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
        _want_tr: ItemKey,
        subject: Side<'c>,
        trait_ref: Side<'c>,
        has_subject: bool,
        depth: u32,
        visited: &mut [ElabKey; MAX_ELAB_VISITED],
        vlen: &mut usize,
    ) -> Result<bool, Refused> {
        let env_mark = self.envs.len();
        let (sub_chain, sub_clen) = self.alias_self_chain(subject);
        let mut stack: [Option<(ItemKey, Side<'c>, Side<'c>, bool, u32)>; MAX_ELAB_STACK] = [None; MAX_ELAB_STACK];
        let mut sp = 0usize;
        if let Some(slot) = stack.get_mut(sp) {
            *slot = Some((seed_tr, seed_self, seed_trait, seed_has_self, depth));
            sp += 1;
        }
        while sp > 0 {
            sp -= 1;
            let Some((cur_tr, cur_self, cur_trait, cur_has_self, cur_depth)) = stack.get_mut(sp).and_then(Option::take) else {
                continue;
            };
            let is_sub = self.erased_sides(cur_self, subject);
            let visit_key = (cur_tr, cur_self.ty, is_sub);
            if cur_depth >= MAX_ELAB_DEPTH || *vlen >= MAX_ELAB_VISITED || visited.iter().take(*vlen).any(|&k| k == visit_key) {
                continue;
            }
            if let Some(slot) = visited.get_mut(*vlen) {
                *slot = visit_key;
                *vlen += 1;
            } else {
                continue;
            }
            let Some((tenv, views)) = self.trait_env_from_seed(cur_tr, cur_self, cur_trait, cur_has_self)? else {
                continue;
            };
            for allow_alias in [false, true] {
                for cv in views {
                    let c = Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env: tenv } };
                    if self.match_trait_clause(c, cv.binders, subject, trait_ref, has_subject, allow_alias)? {
                        return Ok(true);
                    }
                }
            }
            let next_depth = cur_depth + 1;
            if next_depth < MAX_ELAB_DEPTH {
                let start = sp;
                for cv in views {
                    let c = Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env: tenv } };
                    if let Ok(PNode::ExTrait(ct, _)) = self.node(c)
                        && self.fits(c).is_ok()
                        && let Ok(c_own) = self.own(c)
                        && let Ok(ckids) = self.kids_of(c)
                        && ckids.len() > 0
                        && let Some((_, crest)) = c.slots.split(c_own)
                        && let Ok((c_self, _)) = self.kid(c, &ckids, crest, 0)
                        && (self.erased_sides(c_self, subject)
                            || self.erased_sides(c_self, cur_self)
                            || sub_chain.iter().take(sub_clen).flatten().any(|&a| self.erased_sides(c_self, a)))
                    {
                        let c = self.instantiate_clause_side(c, cv.binders)?;
                        let (_, crest) = c.slots.split(c_own).ok_or(self.malformed(c.ty, Bad::StepBound))?;
                        let (c_self, _) = self.kid(c, &ckids, crest, 0)?;
                        if let Some(slot) = stack.get_mut(sp) {
                            *slot = Some((ct, c_self, c, true, next_depth));
                            sp += 1;
                        }
                    }
                }
                if let Some(pushed) = stack.get_mut(start..sp) {
                    pushed.reverse();
                }
            }
        }
        self.truncate_envs(env_mark);
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
        visited: &mut [ElabKey; MAX_ELAB_VISITED],
        vlen: &mut usize,
    ) -> Result<Option<Side<'c>>, Refused> {
        let env_mark = self.envs.len();
        let (sub_chain, sub_clen) = self.alias_self_chain(subject);
        let mut stack: [Option<(ItemKey, Side<'c>, Side<'c>, bool, u32)>; MAX_ELAB_STACK] = [None; MAX_ELAB_STACK];
        let mut sp = 0usize;
        if let Some(slot) = stack.get_mut(sp) {
            *slot = Some((seed_tr, seed_self, seed_trait, seed_has_self, depth));
            sp += 1;
        }
        while sp > 0 {
            sp -= 1;
            let Some((cur_tr, cur_self, cur_trait, cur_has_self, cur_depth)) = stack.get_mut(sp).and_then(Option::take) else {
                continue;
            };
            let is_sub = self.erased_sides(cur_self, subject);
            let visit_key = (cur_tr, cur_self.ty, is_sub);
            if cur_depth >= MAX_ELAB_DEPTH || *vlen >= MAX_ELAB_VISITED || visited.iter().take(*vlen).any(|&k| k == visit_key) {
                continue;
            }
            if let Some(slot) = visited.get_mut(*vlen) {
                *slot = visit_key;
                *vlen += 1;
            } else {
                continue;
            }
            let Some((tenv, views)) = self.trait_env_from_seed(cur_tr, cur_self, cur_trait, cur_has_self)? else {
                continue;
            };
            for allow_alias in [false, true] {
                for cv in views {
                    let c = Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env: tenv } };
                    if let Some(term) = self.match_proj_clause(c, cv.binders, subject, want, want_arg_count, want_tr, want_name, allow_alias)? {
                        return Ok(Some(term));
                    }
                }
            }
            let next_depth = cur_depth + 1;
            if next_depth < MAX_ELAB_DEPTH {
                let start = sp;
                for cv in views {
                    let c = Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env: tenv } };
                    if let Ok(PNode::ExTrait(ct, _)) = self.node(c)
                        && self.fits(c).is_ok()
                        && let Ok(c_own) = self.own(c)
                        && let Ok(ckids) = self.kids_of(c)
                        && ckids.len() > 0
                        && let Some((_, crest)) = c.slots.split(c_own)
                        && let Ok((c_self, _)) = self.kid(c, &ckids, crest, 0)
                        && (self.erased_sides(c_self, subject)
                            || self.erased_sides(c_self, cur_self)
                            || sub_chain.iter().take(sub_clen).flatten().any(|&a| self.erased_sides(c_self, a)))
                    {
                        let c = self.instantiate_clause_side(c, cv.binders)?;
                        let (_, crest) = c.slots.split(c_own).ok_or(self.malformed(c.ty, Bad::StepBound))?;
                        let (c_self, _) = self.kid(c, &ckids, crest, 0)?;
                        if let Some(slot) = stack.get_mut(sp) {
                            *slot = Some((ct, c_self, c, true, next_depth));
                            sp += 1;
                        }
                    }
                }
                if let Some(pushed) = stack.get_mut(start..sp) {
                    pushed.reverse();
                }
            }
        }
        self.truncate_envs(env_mark);
        Ok(None)
    }

    /// Collect up to 16 ancestor `Self` subjects of a nested `PNode::Alias` (e.g. for
    /// `<<NodeAdapter<N, C> as FullNodeTypes>::Provider as DatabaseProviderFactory>::Provider`,
    /// or `<<RpcNodeCoreFor<N> as RpcNodeCore>::Evm as ConfigureEvm>::BlockExecutorFactory` where
    /// `<RpcNodeCoreFor<N> as RpcNodeCore>::Evm == <N as FullNodeComponents>::Evm`), walking BOTH
    /// the raw ancestor chain (`RpcNodeCoreFor<N>`) and any normalized alias's ancestor chain
    /// (`N`) so caller bounds on either side remain reachable.
    fn alias_self_chain(&mut self, subject: Side<'c>) -> ([Option<Side<'c>>; 16], usize) {
        let mut out: [Option<Side<'c>>; 16] = [None; 16];
        let mut len = 0usize;
        let mut queue: [Option<Side<'c>>; 16] = [None; 16];
        let mut qhead = 0usize;
        let mut qtail = 0usize;
        if let Some(slot) = queue.get_mut(qtail) {
            *slot = Some(subject);
            qtail += 1;
        }
        while qhead < qtail && len < out.len() {
            let Some(cur) = queue.get(qhead).copied().flatten() else { break };
            qhead += 1;
            // Reverse projection equality: if a caller `where`-clause states
            // `<X as Tr>::Assoc == cur` (e.g. `RpcNodeCoreFor<N>: RpcNodeCore<Evm = N::Evm>`
            // alongside `RpcNodeCoreFor<N>: TempoEthApiBounds`), then `X` (`RpcNodeCoreFor<N>`)
            // is also an ancestor whose trait bounds constrain `cur` even after `blaze-typeck`
            // normalized `<X as Tr>::Assoc` to `cur` in body types.
            if !self.in_erased_norm
                && matches!(self.node(cur), Ok(PNode::Alias { .. } | PNode::Param(_)))
                && let Some(oenv) = self.envs.owner_env()
            {
                let owner: &'c dyn Owner = self.owner;
                let clauses = owner.clauses();
                for cv in clauses {
                    let c = Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env: oenv } };
                    if let Ok(PNode::ExProj { .. }) = self.node(c)
                        && self.fits(c).is_ok()
                        && let Ok(c_own) = self.own(c)
                        && let Ok(ckids) = self.kids_of(c)
                        && ckids.len() >= 2
                        && let Some((_, mut crest)) = c.slots.split(c_own)
                        && let Ok((c_self, r0)) = self.kid(c, &ckids, crest, 0)
                    {
                        crest = r0;
                        let mut term = None;
                        for k in 1..ckids.len() {
                            let Ok((ck, r)) = self.kid(c, &ckids, crest, k) else {
                                term = None;
                                break;
                            };
                            crest = r;
                            if k + 1 == ckids.len() {
                                term = Some(ck);
                            }
                        }
                        if let Some(term) = term
                            && self.erased_sides(term, cur)
                            && let Ok(Resolved::Side(c_self)) = self.resolve(c_self)
                            && !out.iter().take(len).flatten().any(|&prev| prev.ty == c_self.ty && Self::env_of(prev) == Self::env_of(c_self))
                        {
                            if let Some(slot) = out.get_mut(len) {
                                *slot = Some(c_self);
                                len += 1;
                            }
                            if qtail < queue.len()
                                && let Some(slot) = queue.get_mut(qtail)
                            {
                                *slot = Some(c_self);
                                qtail += 1;
                            }
                        }
                    }
                }
            }
            let Ok(PNode::Alias { .. }) = self.node(cur) else { continue };
            let Ok(own) = self.own(cur) else { continue };
            let Ok(kids) = self.kids_of(cur) else { continue };
            if kids.len() == 0 {
                continue;
            }
            let Some((_, rest)) = cur.slots.split(own) else { continue };
            let Ok((inner, _)) = self.kid(cur, &kids, rest, 0) else { continue };
            let Ok(Resolved::Side(inner)) = self.resolve(inner) else { continue };
            if let Some(slot) = out.get_mut(len) {
                *slot = Some(inner);
                len += 1;
            }
            if qtail < queue.len()
                && let Some(slot) = queue.get_mut(qtail)
            {
                *slot = Some(inner);
                qtail += 1;
            }
            if !self.in_erased_norm && matches!(self.node(inner), Ok(PNode::Alias { .. })) && self.fits(inner).is_ok() {
                let (norm, did) = self.try_normalize_subject_flag(inner, 0);
                if did {
                    if let Some(slot) = out.get_mut(len) {
                        *slot = Some(norm);
                        len += 1;
                    }
                    if qtail < queue.len()
                        && let Some(slot) = queue.get_mut(qtail)
                    {
                        *slot = Some(norm);
                        qtail += 1;
                    }
                }
            }
        }
        (out, len)
    }

    /// Bind an opaque type's defining-item env from a clause whose child 0 is `Opaque(op_key, ..)`
    /// or a projection `Alias` whose self-chain reaches `Opaque(op_key, ..)`.
    fn opaque_clause_side(&mut self, cv: &blaze_typeck::region_facts::ClauseView<'c>, op_lts: u32, subject: Side<'c>, op_key: ItemKey) -> Result<Option<Side<'c>>, Refused> {
        let sub_kids = self.kids_of(subject)?;
        let env_mark = self.envs.len();
        let Some(oenv) = self.envs.impl_env(op_lts, sub_kids.len()) else { return Ok(None) };
        let c = Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env: oenv } };
        if self.fits(c).is_err() {
            self.truncate_envs(env_mark);
            return Ok(None);
        }
        let c = self.instantiate_clause_side(c, cv.binders)?;
        let c_env = Self::env_of(c).unwrap_or(oenv);
        let c_own = self.own(c)?;
        let ckids = self.kids_of(c)?;
        if ckids.len() == 0 {
            self.truncate_envs(env_mark);
            return Ok(None);
        }
        let Some((_, crest)) = c.slots.split(c_own) else {
            self.truncate_envs(env_mark);
            return Ok(None);
        };
        let (c0, _) = self.kid(c, &ckids, crest, 0)?;
        let op_in_clause = if matches!(self.node(c0)?, PNode::Opaque(k, _) if k == op_key) {
            c0
        } else {
            let (chain, clen) = self.alias_self_chain(c0);
            let Some(found) = chain.iter().take(clen).flatten().copied().find(|&a| matches!(self.node(a), Ok(PNode::Opaque(k, _)) if k == op_key)) else {
                self.truncate_envs(env_mark);
                return Ok(None);
            };
            found
        };
        // Walking `op_in_clause` against `subject` binds every captured lifetime and type param of `c_env`.
        if self.walk(op_in_clause, subject, Variance::In, 1).is_err() {
            self.truncate_envs(env_mark);
            return Ok(None);
        }
        for i in 0..op_lts {
            if let Ok(RegAt::Unbound { param, .. }) = self.envs.reg(c_env, Region::Param(i)) {
                let o = self.out.fresh(Kind::Existential);
                let _ = self.envs.bind_reg(c_env, param, Reg::Origin(o));
            }
        }
        Ok(Some(c))
    }

    /// Try to prove `subject: trait_ref` from the opaque clauses of `op_key` (bound against
    /// `op_side`, which is either `subject` itself or an `Opaque` ancestor in `alias_self_chain`).
    fn try_prove_from_opaque_clauses(
        &mut self,
        op_key: ItemKey,
        op_side: Side<'c>,
        subject: Side<'c>,
        trait_ref: Side<'c>,
        has_subject: bool,
        trait_: ItemKey,
    ) -> Result<bool, Refused> {
        let selected: &'c dyn Selected = self.selected;
        let Some((op_lts, views)) = selected.opaque_clauses(op_key) else {
            return Ok(false);
        };
        for allow_alias in [false, true] {
            for cv in views {
                if let Ok(PNode::ExTrait(ct, _)) = self.sig_types.node(cv.col.ty())
                    && ct == trait_
                {
                    let env_mark = self.envs.len();
                    let hit = match self.opaque_clause_side(cv, op_lts, op_side, op_key)? {
                        Some(c) => self.match_trait_clause(c, 0, subject, trait_ref, has_subject, allow_alias)?,
                        None => false,
                    };
                    self.truncate_envs(env_mark);
                    if hit {
                        return Ok(true);
                    }
                }
            }
        }
        let mut visited = [ELAB_KEY_ZERO; MAX_ELAB_VISITED];
        let mut vlen = 0usize;
        for cv in views {
            if let Ok(PNode::ExTrait(ct, _)) = self.sig_types.node(cv.col.ty()) {
                let env_mark = self.envs.len();
                let hit = if let Some(c) = self.opaque_clause_side(cv, op_lts, op_side, op_key)?
                    && let Ok(c_own) = self.own(c)
                    && let Ok(ckids) = self.kids_of(c)
                    && ckids.len() > 0
                    && let Some((_, crest)) = c.slots.split(c_own)
                    && let Ok((c_self, _)) = self.kid(c, &ckids, crest, 0)
                {
                    self.elaborate_for_trait(ct, c_self, c, true, trait_, subject, trait_ref, has_subject, 0, &mut visited, &mut vlen)?
                } else {
                    false
                };
                self.truncate_envs(env_mark);
                if hit {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Try to normalize `want` from the opaque clauses of `op_key` (bound against `op_side`).
    #[allow(clippy::too_many_arguments)]
    fn try_normalize_from_opaque_clauses(
        &mut self,
        op_key: ItemKey,
        op_side: Side<'c>,
        want: Side<'c>,
        want_arg_count: u32,
        subject: Side<'c>,
        trait_: ItemKey,
        name: blaze_typeck::Name,
    ) -> Result<Option<Side<'c>>, Refused> {
        let selected: &'c dyn Selected = self.selected;
        let Some((op_lts, views)) = selected.opaque_clauses(op_key) else {
            return Ok(None);
        };
        for allow_alias in [false, true] {
            for cv in views {
                if let Ok(PNode::ExProj { trait_: ct, name: cn, .. }) = self.sig_types.node(cv.col.ty())
                    && ct == trait_
                    && cn == name
                {
                    let env_mark = self.envs.len();
                    if let Some(c) = self.opaque_clause_side(cv, op_lts, op_side, op_key)?
                        && let Some(term) = self.match_proj_clause(c, 0, subject, want, want_arg_count, trait_, name, allow_alias)?
                    {
                        return Ok(Some(term));
                    }
                    self.truncate_envs(env_mark);
                }
            }
        }
        let mut visited = [ELAB_KEY_ZERO; MAX_ELAB_VISITED];
        let mut vlen = 0usize;
        for cv in views {
            if let Ok(PNode::ExTrait(ct, _)) = self.sig_types.node(cv.col.ty()) {
                let env_mark = self.envs.len();
                if let Some(c) = self.opaque_clause_side(cv, op_lts, op_side, op_key)?
                    && let Ok(c_own) = self.own(c)
                    && let Ok(ckids) = self.kids_of(c)
                    && ckids.len() > 0
                    && let Some((_, crest)) = c.slots.split(c_own)
                    && let Ok((c_self, _)) = self.kid(c, &ckids, crest, 0)
                    && let Some(term) = self.elaborate_for_proj(ct, c_self, c, true, want, want_arg_count, subject, trait_, name, 0, &mut visited, &mut vlen)?
                {
                    return Ok(Some(term));
                }
                self.truncate_envs(env_mark);
            }
        }
        Ok(None)
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
        // 1b. Builtin `async { .. }: Future` and `async { .. }: IntoFuture` (via the blanket
        //     `impl<F: Future> IntoFuture for F`): neither puts any extra region constraint on
        //     an anonymous `PNode::Future`.
        if matches!(self.node(subject)?, PNode::Future(..))
            && (self.selected.is_future_trait(trait_) || self.selected.is_into_future_trait(trait_))
            && self.nothing_to_tie(trait_ref, u32::from(has_subject))?
        {
            return Ok(true);
        }
        // 1c. Builtin `dyn Trait<'b, ..> + 'o: TryAsDynCompatible<'a>` (`#[lang = "try_as_dyn"]`,
        //     `#[rustc_deny_explicit_impl]`, library/core/src/any.rs): ties `'a` invariantly to
        //     the trait object's own lifetime `'o` (slot 0 of `PNode::Dynamic`).
        if self.selected.is_try_as_dyn_trait(trait_)
            && matches!(self.node(subject)?, PNode::Dynamic(_))
        {
            self.fits(trait_ref)?;
            self.fits(subject)?;
            let t_own = self.own(trait_ref)?;
            let tkids = self.kids_of(trait_ref)?;
            let s_own = self.own(subject)?;
            let non_self = if has_subject { tkids.len().saturating_sub(1) } else { tkids.len() };
            if t_own == SlotCount::new(1) && non_self == 0 && s_own == SlotCount::new(1) {
                let a = self.slot(trait_ref, 0)?;
                let o = self.slot(subject, 0)?;
                self.pair(trait_ref.ty, a, o, Variance::In, Self::env_of(trait_ref), Self::env_of(subject))?;
                return Ok(true);
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
                    if self.match_trait_clause(c, cv.binders, subject, trait_ref, has_subject, allow_alias)? {
                        return Ok(true);
                    }
                }
            }
            let mut visited = [ELAB_KEY_ZERO; MAX_ELAB_VISITED];
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
                {
                    let env_mark = self.envs.len();
                    let c = self.instantiate_clause_side(c, cv.binders)?;
                    let (_, crest) = c.slots.split(c_own).ok_or(self.malformed(c.ty, Bad::StepBound))?;
                    let (c_self, _) = self.kid(c, &ckids, crest, 0)?;
                    let hit = self.elaborate_for_trait(ct, c_self, c, true, trait_, subject, trait_ref, has_subject, 0, &mut visited, &mut vlen)?;
                    self.truncate_envs(env_mark);
                    if hit {
                        return Ok(true);
                    }
                }
            }
            // If `subject` is a projection (e.g. `<N as FullNodeTypes>::Provider`), also
            // elaborate caller clauses on its ancestor `Self` (`N: FullNodeComponents`) so
            // subtrait where-clauses constraining associated types (`Self::Provider: Tr`) apply.
            let (roots, rlen) = self.alias_self_chain(subject);
            for root_opt in roots.iter().take(rlen) {
                let Some(root_self) = *root_opt else { continue };
                for cv in clauses {
                    let c = Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env: oenv } };
                    if let Ok(PNode::ExTrait(ct, _)) = self.node(c)
                        && self.fits(c).is_ok()
                        && let Ok(c_own) = self.own(c)
                        && let Ok(ckids) = self.kids_of(c)
                        && ckids.len() > 0
                        && let Some((_, crest)) = c.slots.split(c_own)
                        && let Ok((c_self, _)) = self.kid(c, &ckids, crest, 0)
                        && self.erased_sides(c_self, root_self)
                    {
                        let env_mark = self.envs.len();
                        let c = self.instantiate_clause_side(c, cv.binders)?;
                        let (_, crest) = c.slots.split(c_own).ok_or(self.malformed(c.ty, Bad::StepBound))?;
                        let (c_self, _) = self.kid(c, &ckids, crest, 0)?;
                        let hit = self.elaborate_for_trait(ct, c_self, c, true, trait_, subject, trait_ref, has_subject, 0, &mut visited, &mut vlen)?;
                        self.truncate_envs(env_mark);
                        if hit {
                            return Ok(true);
                        }
                    }
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
                let mut visited = [ELAB_KEY_ZERO; MAX_ELAB_VISITED];
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
                if self.try_prove_from_opaque_clauses(op_key, subject, subject, trait_ref, has_subject, trait_)? {
                    return Ok(true);
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
                        && self.try_prove_from_alias_clauses(at_lts, views, subject, subject, trait_ref, has_subject, trait_)?
                    {
                        return Ok(true);
                    }
                }
                // Also elaborate bounds of any ancestor `Opaque` or `Alias` in `subject`'s
                // self-chain (e.g. `<impl ExecutableTxIterator<Evm> as ExecutableTxTuple>::Tx`,
                // or `<<N as FullNodeTypes>::Provider as BlockReader>::Block`).
                let (chain, clen) = self.alias_self_chain(subject);
                for anc in chain.iter().take(clen).flatten().copied() {
                    match self.node(anc) {
                        Ok(PNode::Opaque(op_key, _)) => {
                            if self.try_prove_from_opaque_clauses(op_key, anc, subject, trait_ref, has_subject, trait_)? {
                                return Ok(true);
                            }
                        }
                        Ok(PNode::Alias { trait_: aat, name: aname, .. }) => {
                            for source in [selected.assoc_clauses(aat, aname), selected.trait_clauses(aat)] {
                                if let Some((at_lts, views)) = source
                                    && !views.is_empty()
                                    && self.fits(anc).is_ok()
                                    && self.try_prove_from_alias_clauses(at_lts, views, anc, subject, trait_ref, has_subject, trait_)?
                                {
                                    return Ok(true);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        Ok(false)
    }

    #[allow(clippy::too_many_arguments)]
    fn try_prove_from_alias_clauses(
        &mut self,
        at_lts: u32,
        views: &'c [blaze_typeck::region_facts::ClauseView<'c>],
        alias_side: Side<'c>,
        subject: Side<'c>,
        trait_ref: Side<'c>,
        has_subject: bool,
        trait_: ItemKey,
    ) -> Result<bool, Refused> {
        let env_mark = self.envs.len();
        let s_own = self.own(alias_side)?;
        let skids = self.kids_of(alias_side)?;
        let lts = at_lts.max(s_own.get());
        let Some(aenv) = self.envs.impl_env(lts, skids.len()) else { return Ok(false) };
        for k in 0..at_lts.min(s_own.get()) {
            match self.slot(alias_side, k)? {
                RegAt::Known(r) => {
                    if self.envs.bind_reg(aenv, k, r).is_err() {
                        self.truncate_envs(env_mark);
                        return Ok(false);
                    }
                }
                RegAt::Unbound { .. } => {
                    self.truncate_envs(env_mark);
                    return Ok(false);
                }
            }
        }
        for k in at_lts.min(s_own.get())..lts {
            let o = self.out.fresh(Kind::Existential);
            let _ = self.envs.bind_reg(aenv, k, Reg::Origin(o));
        }
        let (_, mut srest) = alias_side.slots.split(s_own).ok_or(self.malformed(alias_side.ty, Bad::StepBound))?;
        for k in 0..skids.len() {
            let (kid, sr) = self.kid(alias_side, &skids, srest, k)?;
            srest = sr;
            let _ = self.envs.bind_type(aenv, k, kid);
        }
        for allow_alias in [false, true] {
            for cv in views {
                let c = Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env: aenv } };
                if self.match_trait_clause(c, cv.binders, subject, trait_ref, has_subject, allow_alias)? {
                    return Ok(true);
                }
            }
        }
        let mut visited = [ELAB_KEY_ZERO; MAX_ELAB_VISITED];
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
                && (self.erased_sides(c_self, alias_side) || self.erased_sides(c_self, subject))
            {
                let c = self.instantiate_clause_side(c, cv.binders)?;
                let (_, crest) = c.slots.split(c_own).ok_or(self.malformed(c.ty, Bad::StepBound))?;
                let (c_self, _) = self.kid(c, &ckids, crest, 0)?;
                if self.elaborate_for_trait(ct, c_self, c, true, trait_, subject, trait_ref, has_subject, 0, &mut visited, &mut vlen)? {
                    return Ok(true);
                }
            }
        }
        self.truncate_envs(env_mark);
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
        // 1b. Builtin `<async { .. } as Future>::Output` and `<async { .. } as IntoFuture>::IntoFuture`
        // (via `impl<F: Future> IntoFuture for F { type Output = F::Output; type IntoFuture = F; }`).
        if want_arg_count == 1
            && let PNode::Future(_, output, _) = self.node(subject)?
        {
            if name == blaze_typeck::Name::of("Output") {
                if let Ok(n) = self.types.count(output) {
                    if n == SlotCount::ZERO {
                        return Ok(Some(Side { ty: output, slots: Slots::Caller(crate::SlotRange::EMPTY) }));
                    }
                    if n.get() <= 64
                        && let Some(env) = self.envs.impl_env(n.get(), 0)
                    {
                        let cap_len = subject.slots.len().map_or(0, SlotCount::get);
                        for k in 0..n.get() {
                            let o = self.out.fresh(Kind::Existential);
                            self.envs.bind_reg(env, k, Reg::Origin(o)).map_err(|w| self.malformed(want.ty, w))?;
                            // Discharge `coroutine.rs`'s assumption that every captured region
                            // of the async block outlives every output region slot.
                            for u in 0..cap_len {
                                if let RegAt::Known(cap_r) = self.slot(subject, u)? {
                                    self.outlives(want.ty, cap_r, Reg::Origin(o))?;
                                }
                            }
                        }
                        return Ok(Some(Side { ty: output, slots: Slots::CallerFresh { env, start: 0, len: n } }));
                    }
                }
            } else if name == blaze_typeck::Name::of("IntoFuture") && self.selected.is_into_future_trait(trait_) {
                return Ok(Some(subject));
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
            if let Some(term) = self.match_proj_clause(b, 0, subject, want, want_arg_count, trait_, name, false)? {
                return Ok(Some(term));
            }
        }
        // 3. Caller's `where`-clauses + transitive supertraits.
        if !clauses.is_empty() {
            let oenv = self.envs.owner_env().ok_or(self.malformed(want.ty, Bad::StepBound))?;
            for allow_alias in [false, true] {
                for cv in clauses {
                    let c = Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env: oenv } };
                    if let Some(term) = self.match_proj_clause(c, cv.binders, subject, want, want_arg_count, trait_, name, allow_alias)? {
                        return Ok(Some(term));
                    }
                }
            }
            let mut visited = [ELAB_KEY_ZERO; MAX_ELAB_VISITED];
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
                {
                    let env_mark = self.envs.len();
                    let c = self.instantiate_clause_side(c, cv.binders)?;
                    let (_, crest) = c.slots.split(c_own).ok_or(self.malformed(c.ty, Bad::StepBound))?;
                    let (c_self, _) = self.kid(c, &ckids, crest, 0)?;
                    if let Some(term) = self.elaborate_for_proj(ct, c_self, c, true, want, want_arg_count, subject, trait_, name, 0, &mut visited, &mut vlen)? {
                        return Ok(Some(term));
                    }
                    self.truncate_envs(env_mark);
                }
            }
            let (roots, rlen) = self.alias_self_chain(subject);
            for root_opt in roots.iter().take(rlen) {
                let Some(root_self) = *root_opt else { continue };
                for cv in clauses {
                    let c = Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env: oenv } };
                    if let Ok(PNode::ExTrait(ct, _)) = self.node(c)
                        && self.fits(c).is_ok()
                        && let Ok(c_own) = self.own(c)
                        && let Ok(ckids) = self.kids_of(c)
                        && ckids.len() > 0
                        && let Some((_, crest)) = c.slots.split(c_own)
                        && let Ok((c_self, _)) = self.kid(c, &ckids, crest, 0)
                        && self.erased_sides(c_self, root_self)
                    {
                        let env_mark = self.envs.len();
                        let c = self.instantiate_clause_side(c, cv.binders)?;
                        let (_, crest) = c.slots.split(c_own).ok_or(self.malformed(c.ty, Bad::StepBound))?;
                        let (c_self, _) = self.kid(c, &ckids, crest, 0)?;
                        if let Some(term) = self.elaborate_for_proj(ct, c_self, c, true, want, want_arg_count, subject, trait_, name, 0, &mut visited, &mut vlen)? {
                            return Ok(Some(term));
                        }
                        self.truncate_envs(env_mark);
                    }
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
                                if non_self > 0 {
                                    if let (Some((_, mut pr0)), Some((_, wr0))) = (pred.slots.split(p_own), want.slots.split(w_own))
                                        && let Ok((_, mut wr1)) = self.kid(want, &wkids, wr0, 0)
                                    {
                                        for j in 0..non_self {
                                            let (Ok((pa, pr)), Ok((wa, wr))) = (
                                                self.kid(pred, &pkids, pr0, j),
                                                self.kid(want, &wkids, wr1, j + 1),
                                            ) else {
                                                same = false;
                                                break;
                                            };
                                            pr0 = pr;
                                            wr1 = wr;
                                            if !self.match_clause_arg(pa, wa, allow_alias) {
                                                same = false;
                                                break;
                                            }
                                        }
                                    } else {
                                        same = false;
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
                let mut visited = [ELAB_KEY_ZERO; MAX_ELAB_VISITED];
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
                if let Some(term) = self.try_normalize_from_opaque_clauses(op_key, subject, want, want_arg_count, subject, trait_, name)? {
                    return Ok(Some(term));
                }
            }
            PNode::Alias { trait_: at, name: an, .. } => {
                let selected: &'c dyn Selected = self.selected;
                for source in [selected.assoc_clauses(at, an), selected.trait_clauses(at)] {
                    if let Some((at_lts, views)) = source
                        && !views.is_empty()
                        && self.fits(subject).is_ok()
                        && let Some(term) = self.try_normalize_from_alias_clauses(at_lts, views, subject, subject, want, want_arg_count, trait_, name)?
                    {
                        return Ok(Some(term));
                    }
                }
                let (chain, clen) = self.alias_self_chain(subject);
                for anc in chain.iter().take(clen).flatten().copied() {
                    match self.node(anc) {
                        Ok(PNode::Opaque(op_key, _)) => {
                            if let Some(term) = self.try_normalize_from_opaque_clauses(op_key, anc, want, want_arg_count, subject, trait_, name)? {
                                return Ok(Some(term));
                            }
                        }
                        Ok(PNode::Alias { trait_: anc_tr, name: anc_name, .. }) => {
                            for source in [selected.assoc_clauses(anc_tr, anc_name), selected.trait_clauses(anc_tr)] {
                                if let Some((at_lts, views)) = source
                                    && !views.is_empty()
                                    && self.fits(anc).is_ok()
                                    && let Some(term) = self.try_normalize_from_alias_clauses(at_lts, views, anc, subject, want, want_arg_count, trait_, name)?
                                {
                                    return Ok(Some(term));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        Ok(None)
    }

    #[allow(clippy::too_many_arguments)]
    fn try_normalize_from_alias_clauses(
        &mut self,
        at_lts: u32,
        views: &'c [blaze_typeck::region_facts::ClauseView<'c>],
        alias_side: Side<'c>,
        subject: Side<'c>,
        want: Side<'c>,
        want_arg_count: u32,
        trait_: ItemKey,
        name: blaze_typeck::Name,
    ) -> Result<Option<Side<'c>>, Refused> {
        let env_mark = self.envs.len();
        let s_own = self.own(alias_side)?;
        let skids = self.kids_of(alias_side)?;
        let lts = at_lts.max(s_own.get());
        let Some(aenv) = self.envs.impl_env(lts, skids.len()) else { return Ok(None) };
        for k in 0..at_lts.min(s_own.get()) {
            match self.slot(alias_side, k)? {
                RegAt::Known(r) => {
                    if self.envs.bind_reg(aenv, k, r).is_err() {
                        self.truncate_envs(env_mark);
                        return Ok(None);
                    }
                }
                RegAt::Unbound { .. } => {
                    self.truncate_envs(env_mark);
                    return Ok(None);
                }
            }
        }
        for k in at_lts.min(s_own.get())..lts {
            let o = self.out.fresh(Kind::Existential);
            let _ = self.envs.bind_reg(aenv, k, Reg::Origin(o));
        }
        let (_, mut srest) = alias_side.slots.split(s_own).ok_or(self.malformed(alias_side.ty, Bad::StepBound))?;
        for k in 0..skids.len() {
            let (kid, sr) = self.kid(alias_side, &skids, srest, k)?;
            srest = sr;
            let _ = self.envs.bind_type(aenv, k, kid);
        }
        for allow_alias in [false, true] {
            for cv in views {
                let c = Side { ty: cv.col.ty(), slots: Slots::Sig { col: cv.col.as_slice(), env: aenv } };
                if let Some(term) = self.match_proj_clause(c, cv.binders, subject, want, want_arg_count, trait_, name, allow_alias)? {
                    return Ok(Some(term));
                }
            }
        }
        let mut visited = [ELAB_KEY_ZERO; MAX_ELAB_VISITED];
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
                && (self.erased_sides(c_self, alias_side) || self.erased_sides(c_self, subject))
            {
                let c = self.instantiate_clause_side(c, cv.binders)?;
                let (_, crest) = c.slots.split(c_own).ok_or(self.malformed(c.ty, Bad::StepBound))?;
                let (c_self, _) = self.kid(c, &ckids, crest, 0)?;
                if let Some(term) = self.elaborate_for_proj(ct, c_self, c, true, want, want_arg_count, subject, trait_, name, 0, &mut visited, &mut vlen)? {
                    return Ok(Some(term));
                }
            }
        }
        self.truncate_envs(env_mark);
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

fn walker<'w, 'c, 'k: 'c, A: Allocator + Copy>(
    cx: &'w mut Cx<'k, '_>,
    envs: &'w mut Envs<'c, A>,
    impl_seen: &'w mut Vec<(ItemKey, EnvId), A>,
    item: ItemKey,
) -> Walk<'w, 'c, A> {
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
        in_erased_norm: false,
        impl_seen,
        walk_stack: [None; WALK_STACK_CAP],
        walk_sp: 0,
        comp_stack: [None; COMP_STACK_CAP],
        comp_sp: 0,
    }
}

/// Runs a full call relation (arguments, output, clauses, and well-formedness) using one shared
/// [`Walk`] and reusable `envs` / `impl_seen` buffers.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_call<'c, 'k: 'c, A: Allocator + Copy>(
    cx: &mut Cx<'k, '_>,
    item: ItemKey,
    args: &[Arg],
    inputs: &[Side<'c>],
    output: Side<'c>,
    dest: Arg,
    clauses: &[Clause<'c>],
    envs: &mut Envs<'c, A>,
    impl_seen: &mut Vec<(ItemKey, EnvId), A>,
) -> Result<(), Refused> {
    impl_seen.clear();
    let mut w = walker(cx, envs, impl_seen, item);
    for (a, &input) in args.iter().zip(inputs.iter()) {
        w.budget = MOST_WALK_STEPS;
        w.walk(Side::caller(*a), input, Variance::Co, 0)?;
    }
    w.budget = MOST_WALK_STEPS;
    w.walk(output, Side::caller(dest), Variance::Co, 0)?;
    w.budget = MOST_WALK_STEPS;
    for c in clauses.iter() {
        match *c {
            Clause::Outlives { long, short, at } => w.outlives(at, long, short)?,
            Clause::TypeOutlives { subject, region } => w.components(subject, region, 0)?,
            Clause::Trait { subject, trait_ref } => w.tie(subject, trait_ref)?,
            Clause::TraitOf { trait_ref, binders } => w.tie_of(trait_ref, binders)?,
            Clause::ProjOf { proj, binders } => w.proj_of(proj, binders)?,
        }
    }
    w.budget = MOST_WALK_STEPS;
    for &input in inputs.iter() {
        w.wf(input, 0)?;
    }
    w.wf(output, 0)
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
    let mut impl_seen = Vec::new_in(envs.alloc());
    walker(cx, envs, &mut impl_seen, item).walk(a, b, v, 0)
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
    let mut impl_seen = Vec::new_in(inst.envs.alloc());
    let mut w = walker(cx, &mut inst.envs, &mut impl_seen, item);
    for &input in inst.inputs.iter() {
        w.wf(input, 0)?;
    }
    w.wf(inst.output, 0)
}

/// The instantiated clauses of `inst` turned into outlives constraints.
pub fn clauses<'c, 'k: 'c, A: Allocator + Copy>(cx: &mut Cx<'k, '_>, inst: &mut Inst<'c, A>) -> Result<(), Refused> {
    let item = inst.item;
    let mut impl_seen = Vec::new_in(inst.envs.alloc());
    let mut w = walker(cx, &mut inst.envs, &mut impl_seen, item);
    for c in inst.clauses.iter() {
        match *c {
            Clause::Outlives { long, short, at } => w.outlives(at, long, short)?,
            Clause::TypeOutlives { subject, region } => w.components(subject, region, 0)?,
            Clause::Trait { subject, trait_ref } => w.tie(subject, trait_ref)?,
            Clause::TraitOf { trait_ref, binders } => w.tie_of(trait_ref, binders)?,
            Clause::ProjOf { proj, binders } => w.proj_of(proj, binders)?,
        }
    }
    Ok(())
}
