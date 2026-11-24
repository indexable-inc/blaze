//! A callee's signature instantiated at one call (end-state.md D156; the audit's C4 and C5).
//!
//! THE RULE (rustc's MIR borrowck: every region of a call's substitution is an inference
//! variable of the caller; the Reference, "Generic parameters", "Lifetime elision"):
//! - an EARLY-BOUND lifetime parameter `Param(i)` is the origin at the call's generic argument
//!   slot that carries it ([`GenericArgs::lifetimes`]: the slots of the callee value's own
//!   regions, which the caller made for this call; a turbofish's `'static` is an origin the
//!   caller knows to be `'static`). A trait's lifetimes are a method's LEADING lifetime
//!   parameters (D148's numbering puts the container's first), so a method of
//!   `Deserializer<'de>` takes `'de` from the call too, and the clause `Self: Deserializer<'de>`
//!   ties it to the caller's bound (relate.rs `tie`);
//! - a LATE-BOUND one (`ParamsView::is_late`) is a fresh origin per call;
//! - a type parameter `Param(t)` is the caller's type at [`GenericArgs::types`]`[t]`, with its
//!   slots: the instantiated type is never interned; a walk that meets `Param(t)` on the
//!   signature's side goes on in the caller's type ([`Side`], [`Slots`]);
//! - a closure's or a fn-pointer-like value's free regions are the CALLER's own
//!   ([`crate::Reg::Owner`]), and the binder its signature is under is instantiated fresh per
//!   call; a binder deeper inside an input stays bound ([`crate::Reg::Bound`]).
//!
//! NORMALIZATION (relate.rs `normalize`): an alias whose impl the type checker selected at this
//! call ([`Selected`]) is that impl's `type Name = U;` value, U's column in the impl's numbering,
//! the impl's parameters bound by walking its header against the alias's arguments. An
//! unselected alias keeps its argument slots; it is refused only where normalization is needed
//! (the other side is not the same alias).
#![blaze::sans_io]

use core::alloc::Allocator;

use blaze_flow::Origin;
use blaze_own::Vec;
use blaze_typeck::region_facts::{ClauseView, ColView, Outlives, ParamsView};
use blaze_typeck::regions::Region;
use blaze_typeck::{ItemKey, Mutbl, Name, PTy};

use crate::types::{RegionTypes, SlotCount, SlotRange};
use crate::{Bad, Refused, Reg};

/// A declared type and its region column (in its item's numbering).
#[derive(Clone, Copy, Debug)]
pub struct Typed<'c> {
    pub ty: PTy,
    pub col: &'c [Region],
}

/// A where-clause or bound of a callee, its regions in the callee's numbering.
#[derive(Clone, Copy, Debug)]
pub enum ClauseIn<'c> {
    /// `T: 'r` (`PClause::TypeOutlives`; the subject's column from the clause's `ClauseView`).
    TypeOutlives { subject: Typed<'c>, region: Region },
    /// `'long: 'short` beyond the lifetime parameters' own (`ParamsView::outlives` is read
    /// already; pass only what it does not hold).
    RegionOutlives { long: Region, short: Region },
    /// `Subject: Trait<..>` whose trait reference holds slots (`V: Visitor<'de>`, and a
    /// method's `Self: Trait<'a>`): `trait_ref` an `ExTrait` node and its column.
    Trait { subject: Typed<'c>, trait_ref: Typed<'c> },
    /// `T: Trait<A..>` as a head's clause column lays it (blaze-ladder heads_regions.rs): one
    /// `ExTrait(Trait, [T, A..])` node, the subject its FIRST argument, its column the trait's
    /// lifetime arguments then `T`'s slots then each `A`'s: the form of [`ImplHead::trait_ref`]
    /// too. Tied as [`ClauseIn::Trait`] is, the subject read off the node (relate.rs `tie_of`).
    TraitOf { trait_ref: Typed<'c> },
    /// `<T as Trait<A..>>::N == U` as a head's clause column lays it: one `ExProj { Trait, N,
    /// [T, A..], U }` node, its column EMPTY (a projection clause that writes a region is
    /// refused by the caller by name). Related by relate.rs `proj_of`: nothing where `U` names
    /// no region of the call (its parent `T: Trait<A..>` clause, laid apart, ties the rest);
    /// else refused by name ([`NotSelected::WhereProjection`]).
    ProjOf { proj: Typed<'c> },
}

/// A fn item's signature as its head record lays it (D148 `SigRegions`, split by
/// [`split_fn_col`]): its lifetime parameters, inputs, output and clauses.
#[derive(Clone, Copy, Debug)]
pub struct Sig<'c> {
    pub item: ItemKey,
    pub params: ParamsView<'c>,
    pub inputs: &'c [Typed<'c>],
    pub output: Typed<'c>,
    pub clauses: &'c [ClauseIn<'c>],
}

/// What a value callee is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueKind {
    /// A value of a fn pointer type: its type's column.
    FnPtr,
    /// A `dyn Fn*` object: the object's `ExTrait` arguments tuple and its `Output` term.
    Dyn,
    /// A value of a type parameter `F: Fn*(..) -> ..`: the clause's column.
    Bound,
}

/// Every thing that is called through the one call rule.
#[derive(Clone, Copy, Debug)]
pub enum Callee<'c> {
    /// A fn item.
    Fn(Sig<'c>),
    /// A method of trait `trait_` (its lifetimes are the leading parameters of `sig`, its
    /// `Self` type parameter 0) or of an inherent impl (`trait_` the impl's self type's item).
    Method { trait_: ItemKey, sig: Sig<'c> },
    /// An operator: its trait's method (`a + b` is `Add::add(a, b)`).
    Operator { trait_: ItemKey, sig: Sig<'c> },
    /// A struct or variant constructor: a fn whose inputs are its fields' columns and whose
    /// output is the ADT over its own parameters, every lifetime early-bound.
    Ctor { adt: ItemKey, variant: u32, lifetimes: u32, fields: &'c [Typed<'c>], output: Typed<'c> },
    /// A closure of `owner`: its signature's column (`SigFrom::Clause`), whose `Param` slots
    /// are the owner's and whose `Late { binder, .. }` slots are its own binder's.
    Closure { owner: ItemKey, ordinal: u32, binder: u32, inputs: &'c [Typed<'c>], output: Typed<'c> },
    /// A fn pointer, `dyn Fn*` or `F: Fn*` value of `owner`'s body: from the type's or the
    /// clause's column, in the owner's numbering, `binder` the one its signature is under.
    Value { kind: ValueKind, owner: ItemKey, binder: u32, inputs: &'c [Typed<'c>], output: Typed<'c> },
}

impl Callee<'_> {
    /// The item a refusal names: the callee, or the owner of a closure or a value.
    pub fn item(&self) -> ItemKey {
        match self {
            Callee::Fn(sig) | Callee::Method { sig, .. } | Callee::Operator { sig, .. } => sig.item,
            Callee::Ctor { adt, .. } => *adt,
            Callee::Closure { owner, .. } | Callee::Value { owner, .. } => *owner,
        }
    }
}

/// A caller's value: its type (in the caller's table) and its slots in the caller's origin
/// column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Arg {
    pub ty: PTy,
    pub at: SlotRange,
}

/// The callee's generic arguments at this call.
#[derive(Clone, Copy, Debug)]
pub struct GenericArgs<'g> {
    /// One slot per EARLY-BOUND lifetime parameter, in `Param` order (late-bound ones skipped):
    /// origins the caller made for this call (rustc renumbers a call's substitution fresh).
    pub lifetimes: SlotRange,
    /// One per type parameter, by its `PNode::Param` index (parents first, `Self` 0; lifetimes
    /// are not counted there): the caller's type and the slots the caller made for it.
    pub types: &'g [Arg],
}

/// What a fresh origin is made for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Late {
    /// The callee's late-bound lifetime parameter.
    Param(u32),
    /// A variable of the binder this call instantiates.
    Bound { binder: u32, var: u32 },
}

/// An environment of an instantiation, by index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnvId(u32);

/// Where a side's region slots are.
#[derive(Clone, Copy, Debug)]
pub enum Slots<'c> {
    /// The caller's slots.
    Caller(SlotRange),
    /// A declared column (in `sig_types`), its regions read through environment `env`.
    Sig { col: &'c [Region], env: EnvId },
    /// A caller-table type (in `types`, e.g. a closure's `FnPtr` signature), its regions read
    /// through environment `env`.
    CallerEnv { col: &'c [Region], env: EnvId },
}

impl<'c> Slots<'c> {
    /// How many slots.
    pub fn len(self) -> Option<SlotCount> {
        match self {
            Slots::Caller(r) => Some(r.count()),
            Slots::Sig { col, .. } | Slots::CallerEnv { col, .. } => SlotCount::of_len(col.len()),
        }
    }

    /// The first `n` and the rest.
    pub fn split(self, n: SlotCount) -> Option<(Slots<'c>, Slots<'c>)> {
        match self {
            Slots::Caller(r) => r.split(n).map(|(a, b)| (Slots::Caller(a), Slots::Caller(b))),
            Slots::Sig { col, env } => {
                if n.len() > col.len() {
                    return None;
                }
                let (a, b) = col.split_at(n.len());
                Some((Slots::Sig { col: a, env }, Slots::Sig { col: b, env }))
            }
            Slots::CallerEnv { col, env } => {
                if n.len() > col.len() {
                    return None;
                }
                let (a, b) = col.split_at(n.len());
                Some((Slots::CallerEnv { col: a, env }, Slots::CallerEnv { col: b, env }))
            }
        }
    }
}

/// One side of a relation: a type and where its slots are.
#[derive(Clone, Copy, Debug)]
pub struct Side<'c> {
    pub ty: PTy,
    pub slots: Slots<'c>,
}

impl Side<'_> {
    /// A caller's value.
    pub fn caller(a: Arg) -> Side<'static> {
        Side { ty: a.ty, slots: Slots::Caller(a.at) }
    }
}

/// An instantiated clause.
#[derive(Clone, Copy, Debug)]
pub enum Clause<'c> {
    /// `long: short`; `at` the node a refusal names.
    Outlives { long: Reg, short: Reg, at: PTy },
    /// Every slot of `subject` outlives `region` (its outlives components).
    TypeOutlives { subject: Side<'c>, region: Reg },
    /// `subject: trait_ref`, whose regions are tied to the impl or the caller's bound that
    /// proves it.
    Trait { subject: Side<'c>, trait_ref: Side<'c> },
    /// [`ClauseIn::TraitOf`]: the subject is `trait_ref`'s first argument.
    TraitOf { trait_ref: Side<'c> },
    /// [`ClauseIn::ProjOf`].
    ProjOf { proj: Side<'c> },
}

#[derive(Clone, Copy, Debug)]
enum Mode {
    /// A callee's or an impl's own numbering: `Param(i)` is `regs[i]`; `bindable` for an
    /// impl's, whose parameters a header walk binds; `gat_base` is where a GAT's own
    /// lifetimes (`Region::Late { binder: u32::MAX, var }`) start inside `regs`.
    Item { bindable: bool, gat_base: u32 },
    /// The caller's numbering: `Param(i)` is the caller's own; `binder`'s variables are
    /// instantiated (`late`).
    Owner { binder: Option<u32> },
}

struct Env<'c, A: Allocator> {
    mode: Mode,
    regs: Vec<Option<Reg>, A>,
    late: Vec<(u32, u32, Origin), A>,
    types: Vec<Option<Side<'c>>, A>,
}

/// A region slot read through its environment.
#[derive(Clone, Copy, Debug)]
pub(crate) enum RegAt {
    Known(Reg),
    /// An impl's lifetime parameter its header walk has not bound yet.
    Unbound { env: EnvId, param: u32 },
}

/// A type parameter read through its environment.
#[derive(Clone, Copy, Debug)]
pub(crate) enum TypeAt<'c> {
    Bound(Side<'c>),
    /// The caller's own type parameter, or a closure owner's: a leaf.
    Rigid,
    /// An impl's type parameter its header walk has not bound yet.
    Unbound { env: EnvId, param: u32 },
}

/// The environments of one call: the callee's, and one per impl a normalization or a bound
/// read (most a handful).
pub struct Envs<'c, A: Allocator> {
    envs: Vec<Env<'c, A>, A>,
    alloc: A,
}

/// The most environments one call makes (rule 18; deeply chained iterator adaptors like
/// `a.flatten().chain(b.flatten()).chain(c.flatten())` open several hundred environments).
pub const MOST_ENVS: usize = 4096;

impl<'c, A: Allocator + Copy> Envs<'c, A> {
    pub fn new_in(alloc: A) -> Envs<'c, A> {
        Envs { envs: Vec::new_in(alloc), alloc }
    }

    fn push(&mut self, mode: Mode, regs: Vec<Option<Reg>, A>, types: Vec<Option<Side<'c>>, A>) -> Option<EnvId> {
        if self.envs.len() >= MOST_ENVS {
            return None;
        }
        let id = EnvId(self.envs.len() as u32);
        self.envs.push(Env { mode, regs, late: Vec::new_in(self.alloc), types });
        Some(id)
    }

    /// A fresh environment for an impl of `lifetimes` lifetime and `types` type parameters,
    /// every one unbound.
    pub(crate) fn impl_env(&mut self, lifetimes: u32, types: u32) -> Option<EnvId> {
        let regs = Vec::from_elem_in(None, lifetimes as usize, self.alloc);
        let tys = Vec::from_elem_in(None, types as usize, self.alloc);
        self.push(Mode::Item { bindable: true, gat_base: lifetimes }, regs, tys)
    }

    /// A fresh environment for an impl of `impl_lifetimes` + `gat_lifetimes` lifetime parameters
    /// and `types` type parameters: `Region::Late { binder: u32::MAX, var }` reads `regs[impl_lifetimes + var]`.
    pub(crate) fn impl_env_gat(&mut self, impl_lifetimes: u32, gat_lifetimes: u32, types: u32) -> Option<EnvId> {
        let total = impl_lifetimes.checked_add(gat_lifetimes)?;
        let regs = Vec::from_elem_in(None, total as usize, self.alloc);
        let tys = Vec::from_elem_in(None, types as usize, self.alloc);
        self.push(Mode::Item { bindable: true, gat_base: impl_lifetimes }, regs, tys)
    }

    /// An environment in the caller's numbering (a bound of the caller's read for a tie).
    pub(crate) fn owner_env(&mut self) -> Option<EnvId> {
        let regs = Vec::new_in(self.alloc);
        let tys = Vec::new_in(self.alloc);
        self.push(Mode::Owner { binder: None }, regs, tys)
    }

    fn env(&self, e: EnvId) -> Result<&Env<'c, A>, Bad> {
        self.envs.get(e.0 as usize).ok_or(Bad::StepBound)
    }

    /// Region `r` of a column read in `env`.
    pub(crate) fn reg(&self, e: EnvId, r: Region) -> Result<RegAt, Bad> {
        let env = self.env(e)?;
        Ok(match (env.mode, r) {
            (_, Region::Static) => RegAt::Known(Reg::Static),
            (Mode::Item { .. }, Region::Param(i)) => match env.regs.get(i as usize) {
                Some(Some(reg)) => RegAt::Known(*reg),
                Some(None) => RegAt::Unbound { env: e, param: i },
                None => return Err(Bad::LifetimePast { index: i }),
            },
            (Mode::Item { gat_base, .. }, Region::Late { binder: u32::MAX, var }) => {
                let Some(idx) = gat_base.checked_add(var) else {
                    return Err(Bad::LifetimePast { index: u32::MAX });
                };
                match env.regs.get(idx as usize) {
                    Some(Some(reg)) => RegAt::Known(*reg),
                    Some(None) => RegAt::Unbound { env: e, param: idx },
                    None => return Err(Bad::LifetimePast { index: idx }),
                }
            }
            (Mode::Item { .. }, Region::Late { binder, var }) => RegAt::Known(Reg::Bound { binder, var }),
            (Mode::Owner { .. }, Region::Param(i)) => RegAt::Known(Reg::Owner(Region::Param(i))),
            (Mode::Owner { binder: Some(b) }, Region::Late { binder, var }) if binder == b => {
                match env.late.iter().find(|&&(x, v, _)| x == binder && v == var) {
                    Some(&(_, _, o)) => RegAt::Known(Reg::Origin(o)),
                    None => return Err(Bad::LateUnmade { binder, var }),
                }
            }
            (Mode::Owner { .. }, Region::Late { binder, var }) => RegAt::Known(Reg::Bound { binder, var }),
        })
    }

    /// Type parameter `t` read in `env`.
    pub(crate) fn type_of(&self, e: EnvId, t: u32) -> Result<TypeAt<'c>, Bad> {
        let env = self.env(e)?;
        Ok(match env.mode {
            Mode::Owner { .. } => TypeAt::Rigid,
            Mode::Item { bindable, .. } => match env.types.get(t as usize) {
                Some(Some(side)) => TypeAt::Bound(*side),
                Some(None) if bindable => TypeAt::Unbound { env: e, param: t },
                _ => return Err(Bad::TypeArg { index: t }),
            },
        })
    }

    pub(crate) fn bind_reg(&mut self, e: EnvId, param: u32, to: Reg) -> Result<(), Bad> {
        let env = self.envs.get_mut(e.0 as usize).ok_or(Bad::StepBound)?;
        match env.regs.get_mut(param as usize) {
            Some(slot @ None) => {
                *slot = Some(to);
                Ok(())
            }
            Some(Some(_)) => Err(Bad::Unconstrained { index: param }),
            None => Err(Bad::LifetimePast { index: param }),
        }
    }

    pub(crate) fn bind_type(&mut self, e: EnvId, param: u32, to: Side<'c>) -> Result<(), Bad> {
        let env = self.envs.get_mut(e.0 as usize).ok_or(Bad::StepBound)?;
        match env.types.get_mut(param as usize) {
            Some(slot @ None) => {
                *slot = Some(to);
                Ok(())
            }
            _ => Err(Bad::TypeArg { index: param }),
        }
    }
}

/// A callee's signature at one call: its inputs, output and clauses as sides and instantiated
/// clauses, over its environments.
pub struct Inst<'c, A: Allocator> {
    pub item: ItemKey,
    pub inputs: Vec<Side<'c>, A>,
    pub output: Side<'c>,
    pub clauses: Vec<Clause<'c>, A>,
    pub envs: Envs<'c, A>,
}

/// The lifetime parameters a callee is instantiated over.
#[derive(Clone, Copy)]
enum Lifetimes<'c> {
    /// A fn's or a method's: early from the call's slots, late fresh.
    Params(ParamsView<'c>),
    /// A constructor's: `n`, every one early.
    Early(u32),
}

/// `callee`'s signature at this call: early-bound regions from `generics`' slots, late-bound and
/// the instantiated binder's fresh (`fresh`, called once per region in `Param` order, then per
/// binder variable in column order), its clauses instantiated. Normalization is done as the
/// relation meets an alias (relate.rs `normalize`), with the same environments.
pub fn instantiate<'c, A: Allocator + Copy>(
    callee: Callee<'c>,
    generics: GenericArgs<'_>,
    fresh: &mut impl FnMut(Late) -> Origin,
    alloc: A,
) -> Result<Inst<'c, A>, Refused> {
    let item = callee.item();
    let mut envs: Envs<'c, A> = Envs::new_in(alloc);
    let (inputs, output, clauses_in, lifetimes): (&'c [Typed<'c>], Typed<'c>, &'c [ClauseIn<'c>], Option<Lifetimes<'c>>) = match callee {
        Callee::Fn(sig) | Callee::Method { sig, .. } | Callee::Operator { sig, .. } => {
            (sig.inputs, sig.output, sig.clauses, Some(Lifetimes::Params(sig.params)))
        }
        Callee::Ctor { lifetimes, fields, output, .. } => (fields, output, &[], Some(Lifetimes::Early(lifetimes))),
        Callee::Closure { inputs, output, .. } | Callee::Value { inputs, output, .. } => (inputs, output, &[], None),
    };
    let malformed = |node: PTy, what: Bad| Refused::Malformed { item, node, what };
    let e0 = match lifetimes {
        Some(l) => {
            let count = match l {
                Lifetimes::Params(p) => p.count(),
                Lifetimes::Early(n) => n,
            };
            let mut regs: Vec<Option<Reg>, A> = Vec::with_capacity_in(count as usize, alloc);
            let mut early = 0u32;
            for i in 0..count {
                let late = match l {
                    Lifetimes::Params(p) => p.is_late(i).map_err(|_| malformed(output.ty, Bad::LifetimePast { index: i }))?,
                    Lifetimes::Early(_) => false,
                };
                if late {
                    regs.push(Some(Reg::Origin(fresh(Late::Param(i)))));
                } else {
                    let slot = generics.lifetimes.nth(early).ok_or(malformed(output.ty, Bad::LifetimeArgs { want: early + 1, got: generics.lifetimes.count().get() }))?;
                    regs.push(Some(Reg::Slot(slot)));
                    early += 1;
                }
            }
            if early != generics.lifetimes.count().get() {
                return Err(malformed(output.ty, Bad::LifetimeArgs { want: early, got: generics.lifetimes.count().get() }));
            }
            let mut types: Vec<Option<Side<'c>>, A> = Vec::with_capacity_in(generics.types.len(), alloc);
            for a in generics.types {
                types.push(Some(Side::caller(*a)));
            }
            envs.push(Mode::Item { bindable: false, gat_base: count }, regs, types).ok_or(malformed(output.ty, Bad::StepBound))?
        }
        None => {
            let binder = match callee {
                Callee::Closure { binder, .. } | Callee::Value { binder, .. } => binder,
                _ => return Err(malformed(output.ty, Bad::NotOfKind)),
            };
            let regs = Vec::new_in(alloc);
            let types = Vec::new_in(alloc);
            let e = envs.push(Mode::Owner { binder: Some(binder) }, regs, types).ok_or(malformed(output.ty, Bad::StepBound))?;
            // The binder's variables, fresh once each, in column order (inputs, then output).
            let env = envs.envs.get_mut(e.0 as usize).ok_or(malformed(output.ty, Bad::StepBound))?;
            for t in inputs.iter().chain(core::iter::once(&output)) {
                for r in t.col {
                    if let Region::Late { binder: b, var } = *r
                        && b == binder
                        && !env.late.iter().any(|&(x, v, _)| x == b && v == var)
                    {
                        env.late.push((b, var, fresh(Late::Bound { binder: b, var })));
                    }
                }
            }
            e
        }
    };
    let sig_side = |t: Typed<'c>| Side { ty: t.ty, slots: Slots::Sig { col: t.col, env: e0 } };
    let mut ins: Vec<Side<'c>, A> = Vec::with_capacity_in(inputs.len(), alloc);
    for t in inputs {
        ins.push(sig_side(*t));
    }
    let known = |envs: &Envs<'c, A>, r: Region| -> Result<Reg, Refused> {
        match envs.reg(e0, r).map_err(|w| malformed(output.ty, w))? {
            RegAt::Known(reg) => Ok(reg),
            RegAt::Unbound { param, .. } => Err(malformed(output.ty, Bad::Unconstrained { index: param })),
        }
    };
    let mut clauses: Vec<Clause<'c>, A> = Vec::new_in(alloc);
    if let Some(Lifetimes::Params(p)) = lifetimes {
        for &(long, short) in p.outlives() {
            clauses.push(Clause::Outlives { long: known(&envs, long)?, short: known(&envs, short)?, at: output.ty });
        }
    }
    for c in clauses_in {
        clauses.push(match *c {
            ClauseIn::TypeOutlives { subject, region } => Clause::TypeOutlives { subject: sig_side(subject), region: known(&envs, region)? },
            ClauseIn::RegionOutlives { long, short } => Clause::Outlives { long: known(&envs, long)?, short: known(&envs, short)?, at: output.ty },
            ClauseIn::Trait { subject, trait_ref } => Clause::Trait { subject: sig_side(subject), trait_ref: sig_side(trait_ref) },
            ClauseIn::TraitOf { trait_ref } => Clause::TraitOf { trait_ref: sig_side(trait_ref) },
            ClauseIn::ProjOf { proj } => Clause::ProjOf { proj: sig_side(proj) },
        });
    }
    Ok(Inst { item, inputs: ins, output: sig_side(output), clauses, envs })
}

/// A fn's signature column (laid over `fn(inputs..) -> output`, D148) split into its inputs and
/// output, each with its own slots.
pub fn split_fn_col<'c, A: Allocator>(
    types: &dyn RegionTypes,
    item: ItemKey,
    col: ColView<'c>,
    alloc: A,
) -> Result<(Vec<Typed<'c>, A>, Typed<'c>), Refused> {
    let root = col.ty();
    let kids = types.kids(root).map_err(|unknown| Refused::Count { item, unknown })?;
    let n = kids.len();
    if n == 0 || col.children() != n || !col.own().is_empty() {
        let want = SlotCount::new(n);
        let got = SlotCount::new(col.children());
        return Err(Refused::Malformed { item, node: root, what: Bad::ColumnLength { want, got } });
    }
    let typed = |k: u32| -> Result<Typed<'c>, Refused> {
        let ty = kids.get(k).ok_or(Refused::Malformed { item, node: root, what: Bad::NotOfKind })?;
        let slots = col.child(k).ok_or(Refused::Malformed { item, node: root, what: Bad::NotOfKind })?;
        Ok(Typed { ty, col: slots })
    };
    let mut inputs: Vec<Typed<'c>, A> = Vec::with_capacity_in((n - 1) as usize, alloc);
    for k in 0..n - 1 {
        inputs.push(typed(k)?);
    }
    let output = typed(n - 1)?;
    Ok((inputs, output))
}

/// What the call rule asks the type checker's record: which impl decided an alias or a trait
/// bound at THIS call. `written` is the node as the signature (or a normalized impl value)
/// writes it; `name` the associated type, `None` for a trait bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ask {
    pub trait_: ItemKey,
    pub name: Option<Name>,
    pub written: PTy,
}

/// A selected impl's header, in the impl's numbering (`Param(i)` its lifetimes, `PNode::Param`
/// its type parameters): `impl<..> Trait<..> for SelfTy`.
#[derive(Clone, Copy, Debug)]
pub struct ImplHead<'s> {
    pub imp: ItemKey,
    pub lifetimes: u32,
    pub types: u32,
    /// `SelfTy: Trait<A..>` SUBJECT-FIRST, as the head record lays it (blaze-ladder
    /// heads_regions.rs `header_trait`) and as a clause column is laid ([`ClauseIn::TraitOf`]):
    /// one `ExTrait(Trait, [SelfTy, A..])` node, its column the trait's lifetime arguments, then
    /// `SelfTy`'s slots, then each `A`'s. ONE form for a trait reference with its subject: the
    /// self type is its first child, read nowhere else.
    pub trait_ref: Typed<'s>,
}

/// A selected impl's `type Name = U;`: U and its column in the impl's numbering, a GAT's own
/// lifetimes numbered after the impl's.
#[derive(Clone, Copy, Debug)]
pub struct ImplValue<'s> {
    pub value: Typed<'s>,
    pub gat_lifetimes: u32,
}

/// Why no impl decided an alias or a bound at a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotSelected {
    /// The arguments name the caller's generics: decided per instance, not at the call.
    Generic,
    /// Proven by a builtin rule or a global where-clause: no impl text gives a value.
    NotImpl,
    /// Selecting needed a fact the check had not read.
    Unanswered,
    /// The subject is the caller's type parameter and the caller has no such bound.
    NoBound,
    /// The type checker selected an impl, and that impl's header regions (its lifetime
    /// parameters, its self type's and trait reference's columns, the asked value's column)
    /// were not read: the impl is named and its regions are not known here.
    HeadUnread,
    /// Two rows under ONE key (the trait, the name and the whole erased argument list, as far
    /// as [`AskArgs`] tells them apart) name two different answers: no answer, never a pick.
    TwoSources,
    /// The arguments are ground, no impl holds, and the caller assumes no where-clause past
    /// `Sized` (the row's `global_env`): a builtin rule proves it (`Sized`, `Copy`/`Clone` of a
    /// tuple, an array, a fn pointer or a closure, ..). The Reference, "Special types and
    /// traits". No impl text: the rule relates its regions only where the trait has none.
    Builtin,
    /// The type checker named no source: the arguments name the caller's generics, an opaque
    /// type, a closure or a trait object (blaze-typeck selected_rows.rs `ground`). Split by the
    /// head of `Self`, the argument the candidate kinds turn on, until the record names the
    /// source itself (the census's ranking; the typeck scratch edit).
    GenericSelf(SelfHead),
    /// The selected impl has a where-clause `<X as Tr>::N == U` (an `ExProj` clause column):
    /// no rule relates a projection equality yet, so the impl's proof is not related whole.
    WhereProjection,
}

/// A selected impl's where-clauses, in the impl's numbering. blaze-ladder heads.rs `impl_`
/// files them into the impl's head record (heads_regions.rs `impl_header_regions` lays the
/// header's columns, then `fill_clauses` the where clauses after them, as a fn's);
/// they are read by `RegionFacts::clauses(imp)`, `outlives(imp)` and
/// `lifetime_params(imp).outlives()`. Relating an impl's header without them would take the
/// impl's proof for free (relate.rs `impl_where`).
#[derive(Clone, Copy, Debug)]
pub struct ImplWhere<'s> {
    /// The declared `'a: 'b` among the impl's lifetime parameters.
    pub params: &'s [(Region, Region)],
    /// `T: 'r` (naming a clause of `views` by its `nth`) and `'a: 'b` beyond `params`.
    pub outlives: &'s [Outlives],
    /// The clause columns: `T: 'r` subjects, and `T: Tr<..>` laid subject-first.
    pub views: &'s [ClauseView<'s>],
}

impl ImplWhere<'_> {
    /// An impl with no where-clause.
    pub const NONE: ImplWhere<'static> = ImplWhere { params: &[], outlives: &[], views: &[] };
}

/// The head of a selection row's `Self` argument where the type checker named no source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SelfHead {
    /// The caller's type parameter: its where-clauses, or a blanket impl.
    Param,
    /// An opaque type: its own bounds, or a blanket impl.
    Opaque,
    /// A trait object: its principal or projection bounds, or an impl for `dyn ..`.
    Object,
    /// A closure, an async block's future or a coroutine: a builtin rule, or a blanket impl.
    Closure,
    /// An unnormalized alias.
    Alias,
    /// Any other head (an ADT, a reference, a tuple ..) whose arguments name the caller's
    /// generics: a generic impl the record did not name.
    Other,
    /// The row's arguments were not read.
    Unread,
}

/// How one asked argument compares with a row's, every region erased.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keyed {
    Same,
    Differ,
    /// Not decided by structure: an alias, an opaque type, a hole, an impl parameter not yet
    /// bound, a count not read, or the comparison's step bound.
    Unknown,
}

/// The outer constructor shape of a trait impl's `Self` type (used to match projections on
/// the caller's own enclosing trait impl `Self`, e.g. `Zip<A, B>::Item` or `(&T0, &T1)::Item`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelfShape {
    Adt(ItemKey),
    Ref(Mutbl),
    Tuple(u32),
}

/// The asked alias's or bound's arguments `[Self, trait args..]` at the call, as the walk reads
/// them (relate.rs `Keyer`): what a selection row is keyed against.
pub trait AskArgs {
    /// How many arguments; `None` where the asked node's children were not read.
    fn count(&self) -> Option<u32>;
    /// The `k`-th argument against `ty`, a type of the CALLER's table (a row's argument).
    fn compare(&self, k: u32, ty: PTy) -> Keyed;
    /// If argument 0 (`Self`) resolves to an `Adt`, `Ref`, or `Tuple`, its outer shape.
    fn self_shape(&self) -> Option<SelfShape> {
        None
    }
}

/// An ask with no arguments to key by: every comparison [`Keyed::Unknown`].
pub struct Unkeyed;

impl AskArgs for Unkeyed {
    fn count(&self) -> Option<u32> {
        None
    }
    fn compare(&self, _: u32, _: PTy) -> Keyed {
        Keyed::Unknown
    }
}

/// The answer to an [`Ask`].
#[derive(Clone, Copy, Debug)]
pub enum Selection<'s> {
    Impl { head: ImplHead<'s>, value: Option<ImplValue<'s>> },
    Not(NotSelected),
}

/// The one question the call rule asks of the type checker's selections (`Checked::selected`,
/// blaze-typeck selected.rs): never selected again here (rule 77).
pub trait Selected {
    fn select(&self, ask: Ask) -> Selection<'_>;
    /// [`Selected::select`] keyed by the ask's whole erased trait reference: a row whose
    /// arguments `args` proves different does not answer, and rows that are not told apart
    /// must agree ([`NotSelected::TwoSources`]). The walk asks only this. The provided body is
    /// for tables with one row per trait and name (blaze-borrow's tests' stub), where the key
    /// can tell nothing apart; the type checker's table (blaze-flow-lower one_call.rs
    /// `CallSel`) keys.
    fn select_keyed(&self, ask: Ask, args: &dyn AskArgs) -> Selection<'_> {
        let _ = args;
        self.select(ask)
    }
    /// The where-clauses of an impl this table selected ([`ImplWhere`]); `Err` names why they
    /// were not read. The provided body answers [`ImplWhere::NONE`]: blaze-borrow's tests'
    /// stub, whose fixture impls have no where-clause. The type checker's table
    /// (blaze-flow-lower one_call.rs `CallSel`) reads the head record. The body goes when the
    /// stub answers it itself (scratch call-census-5).
    fn impl_where(&self, imp: ItemKey) -> Result<ImplWhere<'_>, NotSelected> {
        let _ = imp;
        Ok(ImplWhere::NONE)
    }
    /// Whether EVERY proof of `trait_` relates no region, whatever its instance: no impl may be
    /// written for it and its builtin rule reads no lifetime (`Sized` and its kin, `Destruct`
    /// outside a const context: core's `#[rustc_deny_explicit_impl]` traits whose rule is
    /// structural). Such a clause asks no selection (relate.rs `tie_of`). The provided body
    /// answers `false`, not known: the clause is then tied through what selected it, or refused
    /// by name (stricter, never looser). The type checker's table answers from the lowering's
    /// facts (blaze-flow-lower `Facts::trait_relates_nothing`).
    fn relates_nothing(&self, trait_: ItemKey) -> bool {
        let _ = trait_;
        false
    }
    /// Whether `<T as trait_>::name` is a compiler-builtin associated type whose normalized value
    /// always carries zero lifetimes (`DiscriminantKind::Discriminant`, `Pointee::Metadata`).
    fn is_zero_slot_builtin_assoc(&self, trait_: ItemKey, name: Name) -> bool {
        let _ = (trait_, name);
        false
    }
    /// Whether `trait_` is one of the `Fn` / `FnMut` / `FnOnce` language traits.
    fn is_fn_trait(&self, trait_: ItemKey) -> bool {
        let _ = trait_;
        false
    }
    /// The signature column and where-clauses of a `FnDef` item (a fn, method, or tuple
    /// constructor), for proving builtin `Fn` / `FnMut` / `FnOnce` bounds and `Output`.
    fn fn_def_sig(&self, item: ItemKey) -> Result<(ParamsView<'_>, ColView<'_>, ImplWhere<'_>), NotSelected> {
        let _ = item;
        Err(NotSelected::Builtin)
    }
    /// The declared supertrait and associated-type bound clauses of `trait_` (`regions.clauses(trait_)`),
    /// along with `trait_`'s declared lifetime parameter count (`ParamsView::count()`).
    fn trait_clauses(&self, trait_: ItemKey) -> Option<(u32, &[ClauseView<'_>])> {
        let _ = trait_;
        None
    }
    /// The declared item-bound clauses of associated type `trait_::name` (filed under
    /// `AssocTyDef::item`), along with its total lifetime parameter count (trait + GAT lifetimes).
    fn assoc_clauses(&self, trait_: ItemKey, name: Name) -> Option<(u32, &[ClauseView<'_>])> {
        let _ = (trait_, name);
        None
    }
    /// The declared bound clauses of an opaque type `opaque` (`regions.clauses(opaque)`),
    /// along with its defining item's lifetime parameter count (`regions.lifetime_params(opaque).count()`).
    fn opaque_clauses(&self, opaque: ItemKey) -> Option<(u32, &[ClauseView<'_>])> {
        let _ = opaque;
        None
    }
}
