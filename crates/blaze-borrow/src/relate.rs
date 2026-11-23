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
    /// How many impls' where-clauses are being related around this point ([`Walk::impl_where`]).
    nested: u32,
}

/// The ask's arguments `[Self, trait args..]`, where a side holds them.
#[derive(Clone, Copy)]
enum AskAt<'c> {
    /// An alias, or a bound laid subject-first (`ExTrait(Tr, [T, A..])`): its children.
    Kids(Side<'c>),
    /// A bound with its subject apart (`ExTrait(Tr, [A..])`): the subject, then the children.
    Subject { subject: Side<'c>, of: Side<'c> },
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
        }
    }
}

impl<A: Allocator + Copy> AskArgs for Keyer<'_, '_, '_, A> {
    fn count(&self) -> Option<u32> {
        match self.at {
            AskAt::Kids(s) => self.walk.kids_of(s).ok().map(|k| k.len()),
            AskAt::Subject { of, .. } => self.walk.kids_of(of).ok().and_then(|k| k.len().checked_add(1)),
        }
    }

    fn compare(&self, k: u32, ty: PTy) -> Keyed {
        match self.arg(k) {
            Some(side) => self.walk.erased(side, ty, 0, &self.budget),
            None => Keyed::Unknown,
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
            Slots::Caller(_) => self.types,
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
            Slots::Sig { col, env } => {
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

    /// One slot pair under `v`: `a` the subtype's region, `b` the supertype's, read in the
    /// environments `ea` and `eb` (`None`: the caller's slots). A first meeting of an impl's
    /// parameter binds it; two bound regions are paired by [`Walk::bound_pair`].
    fn pair(&mut self, node: PTy, a: RegAt, b: RegAt, v: Variance, ea: Option<EnvId>, eb: Option<EnvId>) -> Result<(), Refused> {
        let (a, b) = match (a, b) {
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
            return self.bound_pair(node, BoundAt { env: ea, binder: x, var: y }, BoundAt { env: eb, binder: p, var: q });
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
            Slots::Sig { env, .. } => Some(env),
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
        if depth > MOST_WALK_DEPTH || budget.get() == 0 {
            return Keyed::Unknown;
        }
        budget.set(budget.get() - 1);
        let a = match self.resolve(a) {
            Ok(Resolved::Side(s)) => s,
            Ok(Resolved::Unbound { .. }) | Err(_) => return Keyed::Unknown,
        };
        // One table: a node is its type (regions live in columns, never in nodes).
        if let Slots::Caller(_) = a.slots
            && a.ty == b
        {
            return Keyed::Same;
        }
        let (Ok(na), Ok(nb)) = (self.node(a), self.types.node(b)) else { return Keyed::Unknown };
        // What structure does not decide: a hole, an error, an alias not normalized here, an
        // opaque type against anything but the same opaque type.
        let open = |n: PNode| matches!(n, PNode::Infer | PNode::Error | PNode::Alias { .. });
        if open(na) || open(nb) {
            return Keyed::Unknown;
        }
        match (na, nb) {
            (PNode::Opaque(x, _), PNode::Opaque(y, _)) if x != y => return Keyed::Unknown,
            (PNode::Opaque(..), PNode::Opaque(..)) => {}
            (PNode::Opaque(..), _) | (_, PNode::Opaque(..)) => return Keyed::Unknown,
            // A const's type is a node of its own table: compare its bits, then its type.
            (PNode::ConstValue { ty: ta, bits: x }, PNode::ConstValue { ty: tb, bits: y }) => {
                if x != y {
                    return Keyed::Differ;
                }
                return self.erased(Side { ty: ta, slots: a.slots }, tb, depth + 1, budget);
            }
            _ => {}
        }
        if !head_eq(na, nb) {
            return Keyed::Differ;
        }
        let (Ok(ka), Ok(kb)) = (self.kids_of(a), self.types.kids(b)) else { return Keyed::Unknown };
        if ka.len() != kb.len() {
            return Keyed::Differ;
        }
        let mut out = Keyed::Same;
        for k in 0..ka.len() {
            let (Some(x), Some(y)) = (ka.get(k), kb.get(k)) else { return Keyed::Unknown };
            match self.erased(Side { ty: x, slots: a.slots }, y, depth + 1, budget) {
                Keyed::Differ => return Keyed::Differ,
                Keyed::Unknown => out = Keyed::Unknown,
                Keyed::Same => {}
            }
        }
        out
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
        self.step(a.ty, depth)?;
        if depth == 0 {
            // A new relation: its binders are its own.
            self.bound_len = 0;
        }
        let (a, b) = match (self.resolve(a)?, self.resolve(b)?) {
            (Resolved::Side(a), Resolved::Side(b)) => (a, b),
            (Resolved::Unbound { env, param }, Resolved::Side(other)) | (Resolved::Side(other), Resolved::Unbound { env, param }) => {
                return self.envs.bind_type(env, param, other).map_err(|w| self.malformed(other.ty, w));
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
            return Ok(());
        }
        // An alias meets something other than the same alias: normalize it through the impl
        // the type checker selected at this call.
        let same_alias = matches!((na, nb), (PNode::Alias { trait_: t, name: n, .. }, PNode::Alias { trait_: u, name: m, .. }) if t == u && n == m);
        if !same_alias {
            if let PNode::Alias { .. } = na {
                let a = self.normalize(a, na, depth)?;
                return self.walk(a, b, v, depth + 1);
            }
            if let PNode::Alias { .. } = nb {
                let b = self.normalize(b, nb, depth)?;
                return self.walk(a, b, v, depth + 1);
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
        let (_, mut ra) = a.slots.split(own).ok_or(self.malformed(a.ty, Bad::StepBound))?;
        let (_, mut rb) = b.slots.split(own).ok_or(self.malformed(b.ty, Bad::StepBound))?;
        for k in 0..ka.len() {
            let vk = v.then(ta.variance(a.ty, Pos::Kid(k)).map_err(|u| self.count_err(u))?);
            let (ca, na_) = self.kid(a, &ka, ra, k)?;
            let (cb, nb_) = self.kid(b, &kb, rb, k)?;
            ra = na_;
            rb = nb_;
            self.walk(ca, cb, vk, depth + 1)?;
        }
        Ok(())
    }

    /// The selected impl's value for the alias `s` (its node `n`), its parameters bound by
    /// walking the impl's header against the alias.
    fn normalize(&mut self, s: Side<'c>, n: PNode, depth: u32) -> Result<Side<'c>, Refused> {
        let PNode::Alias { trait_, name, .. } = n else { return Err(self.malformed(s.ty, Bad::NotOfKind)) };
        let selected: &'c dyn Selected = self.selected;
        let key = Keyer { walk: &*self, at: AskAt::Kids(s), budget: Cell::new(MOST_WALK_STEPS) };
        let (head, value) = match selected.select_keyed(Ask { trait_, name: Some(name), written: s.ty }, &key) {
            Selection::Impl { head, value: Some(value) } => (head, value),
            Selection::Impl { value: None, .. } => return Err(self.malformed(s.ty, Bad::NoValue)),
            Selection::Not(why) => return Err(Refused::Unselected { item: self.item, node: s.ty, trait_, name: Some(name), why }),
        };
        let ImplValue { value, gat_lifetimes } = value;
        let lifetimes = head.lifetimes.checked_add(gat_lifetimes).ok_or(self.malformed(s.ty, Bad::StepBound))?;
        let env = self.envs.impl_env(lifetimes, head.types).ok_or(self.malformed(s.ty, Bad::StepBound))?;
        self.fits(s)?;
        let own = self.own(s)?;
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
        self.step(s.ty, depth)?;
        let s = match self.resolve(s)? {
            Resolved::Side(s) => s,
            Resolved::Unbound { param, .. } => return Err(self.malformed(s.ty, Bad::Unconstrained { index: param })),
        };
        let n = self.node(s)?;
        match n {
            PNode::Param(p) => {
                // The caller's own type parameter: `ρ_P: 'r`, checked against the caller's bounds.
                let rho = self.owner.param(p).ok_or(self.malformed(s.ty, Bad::NoParamOrigin { param: p }))?;
                return self.outlives(s.ty, Reg::Origin(rho), region);
            }
            PNode::Alias { trait_, name, .. } => {
                let selected: &'c dyn Selected = self.selected;
                let key = Keyer { walk: &*self, at: AskAt::Kids(s), budget: Cell::new(MOST_WALK_STEPS) };
                if let Selection::Impl { value: Some(_), .. } = selected.select_keyed(Ask { trait_, name: Some(name), written: s.ty }, &key) {
                    let value = self.normalize(s, n, depth)?;
                    return self.components(value, region, depth + 1);
                }
                // Not selected: the alias outlives `'r` if its arguments do (RFC 1214's
                // projection rule, its sufficient half).
            }
            PNode::Closure { .. } | PNode::Future(..) if matches!(s.slots, Slots::Caller(_)) => {
                // A body type of the caller: every type parameter its captures hold outlives
                // `'r` (the Reference, "Closure types"; `spawn(|| use_p(p))` with `F: 'static`
                // asks `P: 'static`, E0310 where the caller has no such bound). Its own slots
                // below. Census wave-1119 (b): `(Param(1), Uni(Static))` at `spawn_blocking`.
                let owner: &'c dyn Owner = self.owner;
                let params = owner.body_params(s.ty).ok_or(self.malformed(s.ty, Bad::BodyParamsUnread))?;
                for &p in params {
                    let rho = owner.param(p).ok_or(self.malformed(s.ty, Bad::NoParamOrigin { param: p }))?;
                    self.outlives(s.ty, Reg::Origin(rho), region)?;
                }
            }
            _ => {}
        }
        self.fits(s)?;
        let own = self.own(s)?;
        for k in 0..own.get() {
            match self.slot(s, k)? {
                RegAt::Known(Reg::Bound { .. }) => {}
                RegAt::Known(r) => self.outlives(s.ty, r, region)?,
                RegAt::Unbound { param, .. } => return Err(self.malformed(s.ty, Bad::Unconstrained { index: param })),
            }
        }
        if let PNode::Dynamic(_) = n {
            // `dyn Tr + 'o: 'r` iff `'o: 'r`.
            return Ok(());
        }
        let kids = self.kids_of(s)?;
        let (_, mut rest) = s.slots.split(own).ok_or(self.malformed(s.ty, Bad::StepBound))?;
        for k in 0..kids.len() {
            let (c, r) = self.kid(s, &kids, rest, k)?;
            rest = r;
            self.components(c, region, depth + 1)?;
        }
        Ok(())
    }

    /// The well-formedness of an instantiated signature type at a call (RFC 1214, `WF(&'r T)`
    /// requires `T: 'r`): for each reference `&'r T` in `s`, every component of `T` outlives
    /// `'r`. Descends through references, raw pointers, slices, arrays, pattern types, tuples
    /// and ADTs only: a fn pointer's types are its binder's (their WF is the pointer's own
    /// check), an alias, an opaque type, a trait object and a body type are taken as given (their
    /// well-formedness is the type checker's). A reference whose region a binder inside the
    /// signature binds (`Reg::Bound`) adds nothing here.
    fn wf(&mut self, s: Side<'c>, depth: u32) -> Result<(), Refused> {
        self.step(s.ty, depth)?;
        let s = match self.resolve(s)? {
            Resolved::Side(s) => s,
            Resolved::Unbound { param, .. } => return Err(self.malformed(s.ty, Bad::Unconstrained { index: param })),
        };
        let n = self.node(s)?;
        let descend = matches!(n, PNode::Ref(..) | PNode::Ptr(..) | PNode::Slice(_) | PNode::Array(..) | PNode::Pat(..) | PNode::Tuple(_) | PNode::Record(..) | PNode::Union(_) | PNode::Adt(..));
        if !descend {
            return Ok(());
        }
        self.fits(s)?;
        let own = self.own(s)?;
        let kids = self.kids_of(s)?;
        let (_, mut rest) = s.slots.split(own).ok_or(self.malformed(s.ty, Bad::StepBound))?;
        for k in 0..kids.len() {
            let (c, r) = self.kid(s, &kids, rest, k)?;
            rest = r;
            if let PNode::Ref(..) = n {
                match self.slot(s, 0)? {
                    RegAt::Known(Reg::Bound { .. }) => {}
                    RegAt::Known(region) => self.components(c, region, depth + 1)?,
                    RegAt::Unbound { param, .. } => return Err(self.malformed(s.ty, Bad::Unconstrained { index: param })),
                }
            }
            self.wf(c, depth + 1)?;
        }
        Ok(())
    }

    /// `subject: trait_ref`: its regions tied to what proves it.
    ///
    /// ONE relation (as [`Walk::tie_of`]): its walks share the alpha pairs of the clause's
    /// binder, so each runs at depth 1 after one reset here.
    fn tie(&mut self, subject: Side<'c>, trait_ref: Side<'c>) -> Result<(), Refused> {
        let PNode::ExTrait(trait_, _) = self.node(trait_ref)? else { return Err(self.malformed(trait_ref.ty, Bad::NotOfKind)) };
        self.bound_len = 0;
        let subject = match self.resolve(subject)? {
            Resolved::Side(s) => s,
            Resolved::Unbound { param, .. } => return Err(self.malformed(subject.ty, Bad::Unconstrained { index: param })),
        };
        if let PNode::Param(p) = self.node(subject)? {
            // The caller's own parameter: the caller's bound proves it.
            let owner: &'c dyn crate::relate::Owner = self.owner;
            let Some(bound) = owner.bound(p, trait_) else {
                return Err(Refused::Unselected { item: self.item, node: trait_ref.ty, trait_, name: None, why: NotSelected::NoBound });
            };
            let env = self.envs.owner_env().ok_or(self.malformed(trait_ref.ty, Bad::StepBound))?;
            let b = Side { ty: bound.ty, slots: Slots::Sig { col: bound.col, env } };
            return self.walk(trait_ref, b, Variance::In, 1);
        }
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
        let saved = (self.bound, self.bound_len);
        self.nested += 1;
        let res = self.impl_where_in(w, trait_, node, env);
        self.nested -= 1;
        (self.bound, self.bound_len) = saved;
        res
    }

    fn impl_where_in(&mut self, w: crate::inst::ImplWhere<'c>, trait_: ItemKey, node: PTy, env: EnvId) -> Result<(), Refused> {
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
        // Every trait and projection clause, its column empty or not: one that names no region of
        // the call relates nothing ([`Walk::inert`], asked by `tie_of` and `proj_of`), any other
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
            match self.node(side)? {
                PNode::ExTrait(..) => self.tie_of(side)?,
                PNode::ExProj { .. } => self.proj_of(side)?,
                _ => {}
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
        if depth > MOST_WALK_DEPTH || *budget == 0 {
            return Err(self.malformed(s.ty, Bad::StepBound));
        }
        *budget -= 1;
        let s = match self.resolve(s)? {
            Resolved::Side(s) => s,
            // An impl parameter no header walk bound: the proof's choice, which may be anything.
            Resolved::Unbound { .. } => return Ok(false),
        };
        if self.fits(s)? != SlotCount::ZERO {
            return Ok(false);
        }
        match self.node(s)? {
            // The caller's own parameter (its `ρ_P`), or a type not decided.
            PNode::Param(_) | PNode::Infer | PNode::Error => return Ok(false),
            PNode::Closure { .. } | PNode::Future(..) | PNode::Coroutine { .. } => {
                // A body type of the caller with no slot: inert only where its captures hold no
                // caller parameter (`Owner::body_params`, the components rule's answer). A body
                // type in a declared column is never laid there: not inert.
                let Slots::Caller(_) = s.slots else { return Ok(false) };
                let owner: &'c dyn Owner = self.owner;
                return Ok(matches!(owner.body_params(s.ty), Some(p) if p.is_empty()));
            }
            _ => {}
        }
        // No slot under `s` (`fits` above): each child takes none of `rest`.
        let kids = self.kids_of(s)?;
        let mut rest = s.slots;
        for k in 0..kids.len() {
            let (c, r) = self.kid(s, &kids, rest, k)?;
            rest = r;
            if !self.inert(c, depth + 1, budget)? {
                return Ok(false);
            }
        }
        Ok(true)
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
        let (_, mut rest) = proj.slots.split(own).ok_or(self.malformed(proj.ty, Bad::StepBound))?;
        let mut term = None;
        for k in 0..kids.len() {
            let (c, r) = self.kid(proj, &kids, rest, k)?;
            rest = r;
            if k == last {
                term = Some(c);
            }
        }
        let term = term.ok_or(self.malformed(proj.ty, Bad::NotOfKind))?;
        let mut budget = MOST_WALK_STEPS;
        if self.inert(term, 0, &mut budget)? {
            return Ok(());
        }
        Err(Refused::Unselected { item: self.item, node: proj.ty, trait_, name: Some(name), why: NotSelected::WhereProjection })
    }

    /// `T: Trait<A..>` laid subject-first (`ExTrait(Trait, [T, A..])`, a clause column's form):
    /// as [`Walk::tie`], with the subject read off the node. The caller's own bound is in the
    /// same form, so it is walked whole; a selected impl's header is in the impl's form (its
    /// trait reference without `Self`, as [`Walk::normalize`] reads it), so the trait's own slots
    /// and each `A` are walked against it, and `T` against the impl's self type.
    ///
    /// ONE relation: the clause's binder (`F: for<'r> Fn(&'r u8, &'r u8)`) is shared by all its
    /// arguments, so the alpha pairs ([`Walk::bound_pair`]) are reset ONCE here and every walk
    /// below runs at depth 1 (a walk at depth 0 resets them: each argument then paired the
    /// binder's variables afresh, and `'r` could pair with two different variables).
    ///
    /// A clause naming no region of the call ([`Walk::inert`]: `T: Marker` at `T = u8`) relates
    /// nothing and asks no proof; every other is tied, its column empty or not.
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
        self.fits(trait_ref)?;
        let own = self.own(trait_ref)?;
        let kids = self.kids_of(trait_ref)?;
        let (_, rest) = trait_ref.slots.split(own).ok_or(self.malformed(trait_ref.ty, Bad::StepBound))?;
        let (subject, mut rest) = self.kid(trait_ref, &kids, rest, 0)?;
        let subject = match self.resolve(subject)? {
            Resolved::Side(s) => s,
            Resolved::Unbound { param, .. } => return Err(self.malformed(subject.ty, Bad::Unconstrained { index: param })),
        };
        if let PNode::Param(p) = self.node(subject)? {
            let owner: &'c dyn crate::relate::Owner = self.owner;
            let Some(bound) = owner.bound(p, trait_) else {
                return Err(Refused::Unselected { item: self.item, node: trait_ref.ty, trait_, name: None, why: NotSelected::NoBound });
            };
            let env = self.envs.owner_env().ok_or(self.malformed(trait_ref.ty, Bad::StepBound))?;
            let b = Side { ty: bound.ty, slots: Slots::Sig { col: bound.col, env } };
            return self.walk(trait_ref, b, Variance::In, 1);
        }
        let key = Keyer { walk: &*self, at: AskAt::Kids(trait_ref), budget: Cell::new(MOST_WALK_STEPS) };
        let head = match selected.select_keyed(Ask { trait_, name: None, written: trait_ref.ty }, &key) {
            Selection::Impl { head, .. } => head,
            Selection::Not(NotSelected::Builtin) if self.nothing_to_tie(trait_ref, 1)? => return Ok(()),
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
        nested: 0,
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
