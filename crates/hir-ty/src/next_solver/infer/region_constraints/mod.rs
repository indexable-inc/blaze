//! See `README.md`.

use std::ops::Range;
use std::{cmp, fmt, mem};

use blaze_hash::HashMap;
use rustc_index::IndexVec;
use rustc_type_ir::solve::VisibleForLeakCheck;
use rustc_type_ir::{IsRigid, RegionKind, RegionVid, TypeVisitableExt, UniverseIndex};
use tracing::{debug, instrument};

use self::CombineMapType::*;
use super::MemberConstraint;
use crate::Span;
use crate::next_solver::infer::union_find::{UnionFind, UnionFindMark};
use crate::next_solver::infer::unify_key::RegionVariableValue;
use crate::next_solver::{AliasTy, Binder, DbInterner, ParamTy, PlaceholderType, Region, Ty};

mod leak_check;

impl rustc_type_ir::region_constraint::ConstraintSpan for Span {}

/// The region constraint the next solver builds (`-Zassumptions-on-binders` in rustc), each
/// leaf with the span it came from.
pub type SolverRegionConstraint<'db> =
    rustc_type_ir::region_constraint::RegionConstraint<DbInterner<'db>, Span>;

#[derive(Clone, Debug)]
pub struct SolverRegionConstraintData<'db>(SolverRegionConstraint<'db>);

impl Default for SolverRegionConstraintData<'_> {
    fn default() -> Self {
        Self(SolverRegionConstraint::new_true())
    }
}

impl<'db> SolverRegionConstraintData<'db> {
    pub(crate) fn get_constraint(&self) -> SolverRegionConstraint<'db> {
        // clone: `InferCtxtLike` returns it owned; reached only with assumptions on binders.
        self.0.clone()
    }

    #[instrument(level = "debug", skip(self))]
    pub(crate) fn overwrite(&mut self, constraint: SolverRegionConstraint<'db>) {
        self.0 = constraint;
    }
}

#[derive(Debug, Default)]
pub struct RegionConstraintStorage<'db> {
    /// For each `RegionVid`, the corresponding `RegionVariableOrigin`.
    pub(super) var_infos: IndexVec<RegionVid, RegionVariableInfo>,

    pub(super) legacy_data: RegionConstraintData<'db>,

    // FIXME(-Zassumptions-on-binders): rustc means to merge `solver_data` and `legacy_data`
    // into one representation of region constraints.
    pub(super) solver_data: SolverRegionConstraintData<'db>,

    /// The solver constraint each overwrite in the open snapshots replaced, oldest first, for
    /// rollback.
    solver_overwrites: Vec<SolverRegionConstraint<'db>>,

    /// For a given pair of regions (R1, R2), maps to a region R3 that
    /// is designated as their LUB (edges R1 <= R3 and R2 <= R3
    /// exist). This prevents us from making many such regions.
    lubs: CombineMap<'db>,

    /// For a given pair of regions (R1, R2), maps to a region R3 that
    /// is designated as their GLB (edges R3 <= R1 and R3 <= R2
    /// exist). This prevents us from making many such regions.
    glbs: CombineMap<'db>,

    /// The `lubs` and `glbs` entries added in the open snapshots, for rollback.
    combinations: Vec<(CombineMapType, TwoRegions<'db>)>,

    /// When we add a R1 == R2 constraint, we currently add (a) edges
    /// R1 <= R2 and R2 <= R1 and (b) we unify the two regions in this
    /// table. You can then call `shallow_resolve_region_var` early
    /// which will map R1 and R2 to some common region (i.e., either
    /// R1 or R2). This is important when fulfillment, dropck and other such
    /// code is iterating to a fixed point, because otherwise we sometimes
    /// would wind up with a fresh stream of region variables that have been
    /// equated but appear distinct.
    pub(super) unification_table: UnionFind<RegionVid, RegionVariableValue<'db>>,

    /// a flag set to true when we perform any unifications; this is used
    /// to micro-optimize `take_and_reset_data`
    any_unifications: bool,
}

impl Clone for RegionConstraintStorage<'_> {
    fn clone(&self) -> Self {
        RegionConstraintStorage {
            // clone: only `InferCtxt::fork` copies the region tables.
            var_infos: self.var_infos.clone(),
            // clone: only `InferCtxt::fork` copies the region tables.
            legacy_data: self.legacy_data.clone(),
            // clone: only `InferCtxt::fork` copies the region tables.
            solver_data: self.solver_data.clone(),
            // clone: only `InferCtxt::fork` copies the region tables.
            solver_overwrites: self.solver_overwrites.clone(),
            // clone: only `InferCtxt::fork` copies the region tables.
            lubs: self.lubs.clone(),
            // clone: only `InferCtxt::fork` copies the region tables.
            glbs: self.glbs.clone(),
            // clone: only `InferCtxt::fork` copies the region tables.
            combinations: self.combinations.clone(),
            // clone: only `InferCtxt::fork` copies the region tables.
            unification_table: self.unification_table.clone(),
            any_unifications: self.any_unifications,
        }
    }

    /// Into `self`'s tables, keeping their capacity: a warm fork copies without allocating.
    fn clone_from(&mut self, source: &Self) {
        self.var_infos.raw.clone_from(&source.var_infos.raw);
        self.legacy_data.clone_from(&source.legacy_data);
        self.solver_data.clone_from(&source.solver_data);
        self.solver_overwrites.clone_from(&source.solver_overwrites);
        self.lubs.clone_from(&source.lubs);
        self.glbs.clone_from(&source.glbs);
        self.combinations.clone_from(&source.combinations);
        self.unification_table.clone_from(&source.unification_table);
        self.any_unifications = source.any_unifications;
    }
}

impl RegionConstraintStorage<'_> {
    /// Back to `default()`, its capacity kept.
    pub(crate) fn empty(&mut self) {
        let RegionConstraintStorage {
            var_infos,
            legacy_data,
            solver_data,
            solver_overwrites,
            lubs,
            glbs,
            combinations,
            unification_table,
            any_unifications,
        } = self;
        var_infos.raw.clear();
        legacy_data.empty();
        *solver_data = SolverRegionConstraintData::default();
        solver_overwrites.clear();
        lubs.clear();
        glbs.clear();
        combinations.clear();
        unification_table.empty();
        *any_unifications = false;
    }

    /// Bytes its tables hold room for.
    pub(crate) fn held(&self) -> usize {
        self.var_infos.raw.capacity() * size_of::<RegionVariableInfo>()
            + self.legacy_data.held()
            + self.solver_overwrites.capacity() * size_of::<SolverRegionConstraint<'_>>()
            + (self.lubs.capacity() + self.glbs.capacity())
                * size_of::<(TwoRegions<'_>, RegionVid)>()
            + self.combinations.capacity() * size_of::<(CombineMapType, TwoRegions<'_>)>()
            + self.unification_table.held()
    }
}

pub struct RegionConstraintCollector<'db, 'a> {
    storage: &'a mut RegionConstraintStorage<'db>,
    in_snapshot: bool,
}

pub type VarInfos = IndexVec<RegionVid, RegionVariableInfo>;

/// The full set of region constraints gathered up by the collector.
/// Describes constraints between the region variables and other
/// regions, as well as other conditions that must be verified, or
/// assumptions that can be made.
#[derive(Debug, Default)]
pub struct RegionConstraintData<'db> {
    /// Constraints of the form `A <= B`, where either `A` or `B` can
    /// be a region variable (or neither, as it happens).
    pub constraints: RegionConstraints<'db>,

    /// Constraints of the form `R0 member of [R1, ..., Rn]`, meaning that
    /// `R0` must be equal to one of the regions `R1..Rn`. These occur
    /// with `impl Trait` quite frequently.
    pub member_constraints: Vec<MemberConstraint<'db>>,

    /// A "verify" is something that we need to verify after inference
    /// is done, but which does not directly affect inference in any
    /// way.
    ///
    /// An example is a `A <= B` where neither `A` nor `B` are
    /// inference variables.
    pub verifys: Vec<Verify<'db>>,
}

impl Clone for RegionConstraintData<'_> {
    fn clone(&self) -> Self {
        RegionConstraintData {
            // clone: only a fork or a caller taking the data copies it.
            constraints: self.constraints.clone(),
            // clone: only a fork or a caller taking the data copies it.
            member_constraints: self.member_constraints.clone(),
            // clone: only a fork or a caller taking the data copies it.
            verifys: self.verifys.clone(),
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.constraints.clone_from(&source.constraints);
        self.member_constraints.clone_from(&source.member_constraints);
        self.verifys.clone_from(&source.verifys);
    }
}

impl RegionConstraintData<'_> {
    /// Back to `default()`, its capacity kept.
    fn empty(&mut self) {
        self.constraints.truncate(0, 0);
        self.member_constraints.clear();
        self.verifys.clear();
    }

    fn held(&self) -> usize {
        self.constraints.held()
            + self.member_constraints.capacity() * size_of::<MemberConstraint<'_>>()
            + self.verifys.capacity() * size_of::<Verify<'_>>()
    }
}

/// Represents a constraint that influences the inference process.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Constraint<'db> {
    /// A region variable is a subregion of another.
    VarSubVar(RegionVid, RegionVid),

    /// A concrete region is a subregion of region variable.
    RegSubVar(Region<'db>, RegionVid),

    /// A region variable is a subregion of a concrete region. This does not
    /// directly affect inference, but instead is checked after
    /// inference is complete.
    VarSubReg(RegionVid, Region<'db>),

    /// A constraint where neither side is a variable. This does not
    /// directly affect inference, but instead is checked after
    /// inference is complete.
    RegSubReg(Region<'db>, Region<'db>),
}

impl<'db> Constraint<'db> {
    pub fn involves_placeholders(&self) -> bool {
        match self {
            Constraint::VarSubVar(_, _) => false,
            Constraint::VarSubReg(_, r) | Constraint::RegSubVar(r, _) => r.is_placeholder(),
            Constraint::RegSubReg(r, s) => r.is_placeholder() || s.is_placeholder(),
        }
    }

    /// `(sub, sup)`: the constraint is `sub <= sup`.
    pub fn regions(self, interner: DbInterner<'db>) -> (Region<'db>, Region<'db>) {
        let var = |vid| Region::new_var(interner, vid);
        match self {
            Constraint::VarSubVar(a, b) => (var(a), var(b)),
            Constraint::RegSubVar(a, b) => (a, var(b)),
            Constraint::VarSubReg(a, b) => (var(a), b),
            Constraint::RegSubReg(a, b) => (a, b),
        }
    }
}

/// The constraints `sub <= sup` as columns. A row is a tag byte (the kind in the low two bits,
/// whether the leak check sees it in the next two) and two `u32` operands: a variable's index,
/// or for a concrete region its index in `regions`.
#[derive(Default)]
pub struct RegionConstraints<'db> {
    tag: Vec<u8>,
    sub: Vec<u32>,
    sup: Vec<u32>,
    regions: Vec<Region<'db>>,
}

impl Clone for RegionConstraints<'_> {
    fn clone(&self) -> Self {
        RegionConstraints {
            // clone: only a fork or a caller taking the data copies the columns.
            tag: self.tag.clone(),
            // clone: only a fork or a caller taking the data copies the columns.
            sub: self.sub.clone(),
            // clone: only a fork or a caller taking the data copies the columns.
            sup: self.sup.clone(),
            // clone: only a fork or a caller taking the data copies the columns.
            regions: self.regions.clone(),
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.tag.clone_from(&source.tag);
        self.sub.clone_from(&source.sub);
        self.sup.clone_from(&source.sup);
        self.regions.clone_from(&source.regions);
    }
}

const KIND_VAR_SUB_VAR: u8 = 0;
const KIND_REG_SUB_VAR: u8 = 1;
const KIND_VAR_SUB_REG: u8 = 2;
const KIND_REG_SUB_REG: u8 = 3;
const KIND_MASK: u8 = 0b11;
const VISIBILITY_SHIFT: u8 = 2;

impl<'db> RegionConstraints<'db> {
    #[inline]
    pub fn len(&self) -> usize {
        self.tag.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.tag.is_empty()
    }

    /// The constraint at `index`.
    #[inline]
    pub fn get(&self, index: usize) -> Constraint<'db> {
        let (sub, sup) = (self.sub[index], self.sup[index]);
        match self.tag[index] & KIND_MASK {
            KIND_VAR_SUB_VAR => {
                Constraint::VarSubVar(RegionVid::from_u32(sub), RegionVid::from_u32(sup))
            }
            KIND_REG_SUB_VAR => {
                Constraint::RegSubVar(self.regions[sub as usize], RegionVid::from_u32(sup))
            }
            KIND_VAR_SUB_REG => {
                Constraint::VarSubReg(RegionVid::from_u32(sub), self.regions[sup as usize])
            }
            _ => Constraint::RegSubReg(self.regions[sub as usize], self.regions[sup as usize]),
        }
    }

    /// Whether the leak check sees the constraint at `index`.
    #[inline]
    pub fn visible_for_leak_check(&self, index: usize) -> VisibleForLeakCheck {
        match self.tag[index] >> VISIBILITY_SHIFT {
            0 => VisibleForLeakCheck::Yes,
            1 => VisibleForLeakCheck::No,
            _ => VisibleForLeakCheck::Unreachable,
        }
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = Constraint<'db>> + Clone + '_ {
        self.iter_from(0)
    }

    /// The constraints from `start` on: those added since `len()` was `start`.
    pub fn iter_from(
        &self,
        start: usize,
    ) -> impl ExactSizeIterator<Item = Constraint<'db>> + Clone + '_ {
        (start..self.len()).map(|index| self.get(index))
    }

    pub fn iter_with_visibility(
        &self,
    ) -> impl ExactSizeIterator<Item = (Constraint<'db>, VisibleForLeakCheck)> + Clone + '_ {
        (0..self.len()).map(|index| (self.get(index), self.visible_for_leak_check(index)))
    }

    #[cfg(test)]
    pub fn to_vec(&self) -> Vec<Constraint<'db>> {
        self.iter().collect()
    }

    fn push(&mut self, constraint: Constraint<'db>, visible_for_leak_check: VisibleForLeakCheck) {
        let mut region = |region: Region<'db>| {
            let at = u32::try_from(self.regions.len()).expect("too many region constraints");
            self.regions.push(region);
            at
        };
        let (kind, sub, sup) = match constraint {
            Constraint::VarSubVar(a, b) => (KIND_VAR_SUB_VAR, a.as_u32(), b.as_u32()),
            Constraint::RegSubVar(a, b) => (KIND_REG_SUB_VAR, region(a), b.as_u32()),
            Constraint::VarSubReg(a, b) => (KIND_VAR_SUB_REG, a.as_u32(), region(b)),
            Constraint::RegSubReg(a, b) => {
                let a = region(a);
                (KIND_REG_SUB_REG, a, region(b))
            }
        };
        let visibility = match visible_for_leak_check {
            VisibleForLeakCheck::Yes => 0,
            VisibleForLeakCheck::No => 1,
            VisibleForLeakCheck::Unreachable => 2,
        };
        self.tag.push(kind | (visibility << VISIBILITY_SHIFT));
        self.sub.push(sub);
        self.sup.push(sup);
    }

    fn held(&self) -> usize {
        self.tag.capacity() * size_of::<u8>()
            + (self.sub.capacity() + self.sup.capacity()) * size_of::<u32>()
            + self.regions.capacity() * size_of::<Region<'_>>()
    }

    fn truncate(&mut self, len: usize, regions: usize) {
        assert!(self.len() >= len && self.regions.len() >= regions, "added constraints were lost");
        self.tag.truncate(len);
        self.sub.truncate(len);
        self.sup.truncate(len);
        self.regions.truncate(regions);
    }
}

impl fmt::Debug for RegionConstraints<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

#[derive(Debug, Clone)]
pub struct Verify<'db> {
    pub kind: GenericKind<'db>,
    pub region: Region<'db>,
    pub bound: VerifyBound<'db>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub enum GenericKind<'db> {
    Param(ParamTy),
    Placeholder(PlaceholderType<'db>),
    Alias(AliasTy<'db>),
}

/// Describes the things that some `GenericKind` value `G` is known to
/// outlive. Each variant of `VerifyBound` can be thought of as a
/// function:
/// ```ignore (pseudo-rust)
/// fn(min: Region) -> bool { .. }
/// ```
/// where `true` means that the region `min` meets that `G: min`.
/// (False means nothing.)
///
/// So, for example, if we have the type `T` and we have in scope that
/// `T: 'a` and `T: 'b`, then the verify bound might be:
/// ```ignore (pseudo-rust)
/// fn(min: Region) -> bool {
///    ('a: min) || ('b: min)
/// }
/// ```
/// This is described with an `AnyRegion('a, 'b)` node.
#[derive(Debug, Clone)]
pub enum VerifyBound<'db> {
    /// See [`VerifyIfEq`] docs
    IfEq(Binder<'db, VerifyIfEq<'db>>),

    /// Given a region `R`, expands to the function:
    ///
    /// ```ignore (pseudo-rust)
    /// fn(min) -> bool {
    ///     R: min
    /// }
    /// ```
    ///
    /// This is used when we can establish that `G: R` -- therefore,
    /// if `R: min`, then by transitivity `G: min`.
    OutlivedBy(Region<'db>),

    /// Given a region `R`, true if it is `'empty`.
    IsEmpty,

    /// Given a set of bounds `B`, expands to the function:
    ///
    /// ```ignore (pseudo-rust)
    /// fn(min) -> bool {
    ///     exists (b in B) { b(min) }
    /// }
    /// ```
    ///
    /// In other words, if we meet some bound in `B`, that suffices.
    /// This is used when all the bounds in `B` are known to apply to `G`.
    AnyBound(Vec<VerifyBound<'db>>),

    /// Given a set of bounds `B`, expands to the function:
    ///
    /// ```ignore (pseudo-rust)
    /// fn(min) -> bool {
    ///     forall (b in B) { b(min) }
    /// }
    /// ```
    ///
    /// In other words, if we meet *all* bounds in `B`, that suffices.
    /// This is used when *some* bound in `B` is known to suffice, but
    /// we don't know which.
    AllBounds(Vec<VerifyBound<'db>>),
}

/// This is a "conditional bound" that checks the result of inference
/// and supplies a bound if it ended up being relevant. It's used in situations
/// like this:
///
/// ```rust,ignore (pseudo-Rust)
/// fn foo<'a, 'b, T: SomeTrait<'a>>
/// where
///    <T as SomeTrait<'a>>::Item: 'b
/// ```
///
/// If we have an obligation like `<T as SomeTrait<'?x>>::Item: 'c`, then
/// we don't know yet whether it suffices to show that `'b: 'c`. If `'?x` winds
/// up being equal to `'a`, then the where-clauses on function applies, and
/// in that case we can show `'b: 'c`. But if `'?x` winds up being something
/// else, the bound isn't relevant.
///
/// In the [`VerifyBound`], this struct is enclosed in `Binder` to account
/// for cases like
///
/// ```rust,ignore (pseudo-Rust)
/// where for<'a> <T as SomeTrait<'a>::Item: 'a
/// ```
///
/// The idea is that we have to find some instantiation of `'a` that can
/// make `<T as SomeTrait<'a>>::Item` equal to the final value of `G`,
/// the generic we are checking.
///
/// ```ignore (pseudo-rust)
/// fn(min) -> bool {
///     exists<'a> {
///         if G == K {
///             B(min)
///         } else {
///             false
///         }
///     }
/// }
/// ```
#[derive(Debug, Clone)]
pub struct VerifyIfEq<'db> {
    /// Type which must match the generic `G`
    pub ty: Ty<'db>,

    /// Bound that applies if `ty` is equal.
    pub bound: Region<'db>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TwoRegions<'db> {
    a: Region<'db>,
    b: Region<'db>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum CombineMapType {
    Lub,
    Glb,
}

type CombineMap<'db> = HashMap<TwoRegions<'db>, RegionVid>;

#[derive(Debug, Clone)]
pub struct RegionVariableInfo {
    // FIXME: This is only necessary for `fn take_and_reset_data` and
    // `lexical_region_resolve`. We should rework `lexical_region_resolve`
    // in the near/medium future anyways and could move the unverse info
    // for `fn take_and_reset_data` into a separate table which is
    // only populated when needed.
    //
    // For both of these cases it is fine that this can diverge from the
    // actual universe of the variable, which is directly stored in the
    // unification table for unknown region variables. At some point we could
    // stop emitting bidirectional outlives constraints if equate succeeds.
    // This would be currently unsound as it would cause us to drop the universe
    // changes in `lexical_region_resolve`.
    pub universe: UniverseIndex,
    pub span: Span,
}

/// Where the region variables and constraints stood when a snapshot started.
#[derive(Clone, Copy)]
pub(crate) struct RegionSnapshot {
    unification: UnionFindMark,
    vars: u32,
    constraints: u32,
    regions: u32,
    verifys: u32,
    combinations: u32,
    solver_overwrites: u32,
    any_unifications: bool,
}

impl<'db> RegionConstraintStorage<'db> {
    #[inline]
    pub(crate) fn collector(&mut self, in_snapshot: bool) -> RegionConstraintCollector<'db, '_> {
        RegionConstraintCollector { storage: self, in_snapshot }
    }

    pub(crate) fn start_snapshot(&mut self) -> RegionSnapshot {
        debug!("RegionConstraintCollector: start_snapshot");
        RegionSnapshot {
            unification: self.unification_table.start_snapshot(),
            vars: self.var_infos.len() as u32,
            constraints: self.legacy_data.constraints.len() as u32,
            regions: self.legacy_data.constraints.regions.len() as u32,
            verifys: self.legacy_data.verifys.len() as u32,
            combinations: self.combinations.len() as u32,
            solver_overwrites: self.solver_overwrites.len() as u32,
            any_unifications: self.any_unifications,
        }
    }

    pub(crate) fn rollback_to(&mut self, snapshot: RegionSnapshot) {
        debug!("RegionConstraintCollector: rollback_to({:?})", snapshot);
        while self.combinations.len() > snapshot.combinations as usize {
            let (kind, regions) = self.combinations.pop().unwrap();
            match kind {
                Glb => self.glbs.remove(&regions),
                Lub => self.lubs.remove(&regions),
            };
        }
        self.unification_table.rollback_to(snapshot.unification);
        assert!(self.var_infos.len() >= snapshot.vars as usize, "region variables were lost");
        self.var_infos.truncate(snapshot.vars as usize);
        self.legacy_data
            .constraints
            .truncate(snapshot.constraints as usize, snapshot.regions as usize);
        assert!(self.legacy_data.verifys.len() >= snapshot.verifys as usize, "verifys were lost");
        self.legacy_data.verifys.truncate(snapshot.verifys as usize);
        // The first overwrite after the snapshot replaced the constraint the snapshot saw.
        let overwrites = snapshot.solver_overwrites as usize;
        assert!(self.solver_overwrites.len() >= overwrites, "solver overwrites were lost");
        if self.solver_overwrites.len() > overwrites {
            self.solver_overwrites.truncate(overwrites + 1);
            let old = self.solver_overwrites.pop().expect("truncated to at least one");
            self.solver_data.overwrite(old);
        }
        self.any_unifications = snapshot.any_unifications;
    }

    /// `outermost`: no snapshot is open after this one.
    pub(crate) fn commit(&mut self, snapshot: RegionSnapshot, outermost: bool) {
        self.unification_table.commit(snapshot.unification);
        if outermost {
            self.combinations.clear();
            self.solver_overwrites.clear();
        }
    }
}

impl<'db> RegionConstraintCollector<'db, '_> {
    pub fn num_region_vars(&self) -> usize {
        self.storage.var_infos.len()
    }

    pub fn region_constraint_data(&self) -> &RegionConstraintData<'db> {
        &self.storage.legacy_data
    }

    /// Takes (and clears) the current set of constraints. Note that
    /// the set of variables remains intact, but all relationships
    /// between them are reset. This is used during NLL checking to
    /// grab the set of constraints that arose from a particular
    /// operation.
    ///
    /// We don't want to leak relationships between variables between
    /// points because just because (say) `r1 == r2` was true at some
    /// point P in the graph doesn't imply that it will be true at
    /// some other point Q, in NLL.
    ///
    /// Not legal during a snapshot.
    pub fn take_and_reset_data(&mut self) -> RegionConstraintData<'db> {
        assert!(!self.in_snapshot);

        // If you add a new field to `RegionConstraintCollector`, you
        // should think carefully about whether it needs to be cleared
        // or updated in some way.
        let RegionConstraintStorage {
            var_infos,
            legacy_data,
            // FIXME(-Zassumptions-on-binders): rustc does not reset `solver_data` either.
            solver_data: _,
            solver_overwrites: _,
            lubs,
            glbs,
            combinations,
            unification_table,
            any_unifications,
        } = self.storage;

        // Clear the tables of (lubs, glbs), so that we will create
        // fresh regions if we do a LUB operation. As it happens,
        // LUB/GLB are not performed by the MIR type-checker, which is
        // the one that uses this method, but it's good to be correct.
        lubs.clear();
        glbs.clear();
        combinations.clear();

        let data = mem::take(legacy_data);

        // Clear all unifications and recreate the variables a "now
        // un-unified" state. Note that when we unify `a` and `b`, we
        // also insert `a <= b` and a `b <= a` edges, so the
        // `RegionConstraintData` contains the relationship here.
        if *any_unifications {
            *any_unifications = false;
            unification_table.reset_unifications(|vid| {
                let var_info = &var_infos[vid];
                RegionVariableValue::Unknown { universe: var_info.universe, span: var_info.span }
            });
        }

        data
    }

    pub fn legacy_data(&self) -> &RegionConstraintData<'db> {
        &self.storage.legacy_data
    }

    pub fn solver_data(&self) -> &SolverRegionConstraintData<'db> {
        &self.storage.solver_data
    }

    pub fn overwrite_solver_constraint(&mut self, constraint: SolverRegionConstraint<'db>) {
        assert!(
            !constraint.has_escaping_bound_vars(),
            "solver region constraint has escaping bound vars, which is indicative of a bug in how constraints are handled: {constraint:?}",
        );
        self.replace_solver_constraint(constraint);
    }

    pub fn register_solver_constraint(&mut self, constraint: SolverRegionConstraint<'db>) {
        assert!(
            !constraint.has_escaping_bound_vars(),
            "solver region constraint has escaping bound vars, which is indicative of a bug in how constraints are handled: {constraint:?}",
        );
        let new_constraint = SolverRegionConstraint::build_and(
            constraint,
            self.storage.solver_data.get_constraint(),
        );
        // FIXME(-Zassumptions-on-binders): as in rustc, each registration rewrites the whole
        // constraint and keeps the old one for rollback, instead of an incremental change.
        self.replace_solver_constraint(new_constraint);
    }

    fn replace_solver_constraint(&mut self, constraint: SolverRegionConstraint<'db>) {
        let old = mem::replace(&mut self.storage.solver_data.0, constraint);
        if self.in_snapshot {
            self.storage.solver_overwrites.push(old);
        }
    }

    pub(super) fn new_region_var(&mut self, universe: UniverseIndex, span: Span) -> RegionVid {
        let vid = self.storage.var_infos.push(RegionVariableInfo { universe, span });

        let u_vid = self
            .storage
            .unification_table
            .new_key(RegionVariableValue::Unknown { universe, span });
        assert_eq!(vid, u_vid);
        debug!("created new region variable {:?} in {:?}", vid, universe);
        vid
    }

    fn add_constraint(
        &mut self,
        constraint: Constraint<'db>,
        visible_for_leak_check: VisibleForLeakCheck,
    ) {
        // cannot add constraints once regions are resolved
        debug!("RegionConstraintCollector: add_constraint({:?})", constraint);
        self.storage.legacy_data.constraints.push(constraint, visible_for_leak_check);
    }

    pub(super) fn make_eqregion(
        &mut self,
        a: Region<'db>,
        b: Region<'db>,
        visible_for_leak_check: VisibleForLeakCheck,
    ) {
        if a != b {
            // Eventually, it would be nice to add direct support for
            // equating regions.
            self.make_subregion(a, b, visible_for_leak_check);
            self.make_subregion(b, a, visible_for_leak_check);

            match (a.kind(), b.kind()) {
                (RegionKind::ReVar(a), RegionKind::ReVar(b)) => {
                    debug!("make_eqregion: unifying {:?} with {:?}", a, b);
                    if self.storage.unification_table.unify_var_var(a, b).is_ok() {
                        self.storage.any_unifications = true;
                    }
                }
                (RegionKind::ReVar(vid), _) => {
                    debug!("make_eqregion: unifying {:?} with {:?}", vid, b);
                    if self
                        .storage
                        .unification_table
                        .unify_var_value(vid, RegionVariableValue::Known { value: b, span: None })
                        .is_ok()
                    {
                        self.storage.any_unifications = true;
                    };
                }
                (_, RegionKind::ReVar(vid)) => {
                    debug!("make_eqregion: unifying {:?} with {:?}", a, vid);
                    if self
                        .storage
                        .unification_table
                        .unify_var_value(vid, RegionVariableValue::Known { value: a, span: None })
                        .is_ok()
                    {
                        self.storage.any_unifications = true;
                    };
                }
                (_, _) => {}
            }
        }
    }

    #[instrument(skip(self), level = "debug")]
    pub(super) fn make_subregion(
        &mut self,
        sub: Region<'db>,
        sup: Region<'db>,
        visible_for_leak_check: VisibleForLeakCheck,
    ) {
        // cannot add constraints once regions are resolved

        match (sub.kind(), sup.kind()) {
            (RegionKind::ReBound(..), _) | (_, RegionKind::ReBound(..)) => {
                panic!("cannot relate bound region: {sub:?} <= {sup:?}");
            }
            (_, RegionKind::ReStatic) => {
                // all regions are subregions of static, so we can ignore this
            }
            (RegionKind::ReVar(sub_id), RegionKind::ReVar(sup_id)) => {
                if sub_id != sup_id {
                    self.add_constraint(Constraint::VarSubVar(sub_id, sup_id), visible_for_leak_check);
                }
            }
            (_, RegionKind::ReVar(sup_id)) => {
                self.add_constraint(Constraint::RegSubVar(sub, sup_id), visible_for_leak_check);
            }
            (RegionKind::ReVar(sub_id), _) => {
                self.add_constraint(Constraint::VarSubReg(sub_id, sup), visible_for_leak_check);
            }
            _ => {
                if sub != sup {
                    self.add_constraint(Constraint::RegSubReg(sub, sup), visible_for_leak_check);
                }
            }
        }
    }

    pub(super) fn lub_regions(
        &mut self,
        db: DbInterner<'db>,
        origin: Span,
        a: Region<'db>,
        b: Region<'db>,
    ) -> Region<'db> {
        // cannot add constraints once regions are resolved
        debug!("RegionConstraintCollector: lub_regions({:?}, {:?})", a, b);
        #[expect(clippy::if_same_then_else)]
        if a.is_static() || b.is_static() {
            a // nothing lives longer than static
        } else if a == b {
            a // LUB(a,a) = a
        } else {
            self.combine_vars(db, Lub, a, b, origin)
        }
    }

    pub(super) fn glb_regions(
        &mut self,
        db: DbInterner<'db>,
        origin: Span,
        a: Region<'db>,
        b: Region<'db>,
    ) -> Region<'db> {
        // cannot add constraints once regions are resolved
        debug!("RegionConstraintCollector: glb_regions({:?}, {:?})", a, b);
        #[expect(clippy::if_same_then_else)]
        if a.is_static() {
            b // static lives longer than everything else
        } else if b.is_static() {
            a // static lives longer than everything else
        } else if a == b {
            a // GLB(a,a) = a
        } else {
            self.combine_vars(db, Glb, a, b, origin)
        }
    }

    /// Resolves a region var to its value in the unification table, if it exists.
    /// Otherwise, it is resolved to the root `ReVar` in the table.
    pub fn shallow_resolve_region_var(
        &mut self,
        cx: DbInterner<'db>,
        vid: RegionVid,
    ) -> Region<'db> {
        let ut = &mut self.storage.unification_table;
        let root_vid = ut.find(vid);
        match ut.probe_value(root_vid) {
            RegionVariableValue::Known { value, .. } => value,
            RegionVariableValue::Unknown { .. } => Region::new_var(cx, root_vid),
        }
    }

    pub fn try_resolve_region_var(&mut self, vid: RegionVid) -> Result<Region<'db>, UniverseIndex> {
        match self.storage.unification_table.probe_value(vid) {
            RegionVariableValue::Known { value, .. } => Ok(value),
            RegionVariableValue::Unknown { universe, .. } => Err(universe),
        }
    }

    fn combine_map(&mut self, t: CombineMapType) -> &mut CombineMap<'db> {
        match t {
            Glb => &mut self.storage.glbs,
            Lub => &mut self.storage.lubs,
        }
    }

    fn combine_vars(
        &mut self,
        cx: DbInterner<'db>,
        t: CombineMapType,
        a: Region<'db>,
        b: Region<'db>,
        origin: Span,
    ) -> Region<'db> {
        let vars = TwoRegions { a, b };
        if let Some(c) = self.combine_map(t).get(&vars) {
            return Region::new_var(cx, *c);
        }
        let a_universe = self.universe(a);
        let b_universe = self.universe(b);
        let c_universe = cmp::max(a_universe, b_universe);
        let c = self.new_region_var(c_universe, origin);
        self.combine_map(t).insert(vars, c);
        if self.in_snapshot {
            self.storage.combinations.push((t, vars));
        }
        let new_r = Region::new_var(cx, c);
        for old_r in [a, b] {
            match t {
                Glb => self.make_subregion(new_r, old_r, VisibleForLeakCheck::Yes),
                Lub => self.make_subregion(old_r, new_r, VisibleForLeakCheck::Yes),
            }
        }
        debug!("combine_vars() c={:?}", c);
        new_r
    }

    pub fn universe(&mut self, region: Region<'db>) -> UniverseIndex {
        match region.kind() {
            RegionKind::ReStatic
            | RegionKind::ReErased
            | RegionKind::ReLateParam(..)
            | RegionKind::ReEarlyParam(..)
            | RegionKind::ReError(_) => UniverseIndex::ROOT,
            RegionKind::RePlaceholder(placeholder) => placeholder.universe,
            RegionKind::ReVar(vid) => match self.try_resolve_region_var(vid) {
                Ok(value) => self.universe(value),
                Err(universe) => universe,
            },
            RegionKind::ReBound(..) => panic!("universe(): encountered bound region {region:?}"),
        }
    }

    pub fn vars_since_snapshot(&self, value_count: usize) -> (Range<RegionVid>, Vec<Span>) {
        let range =
            RegionVid::from(value_count)..RegionVid::from(self.storage.unification_table.len());
        (
            range.start..range.end,
            (range.start.as_usize()..range.end.as_usize())
                .map(|index| self.storage.var_infos[RegionVid::from_usize(index)].span)
                // alloc: only when fudging, one span per variable made in the snapshot.
                .collect(),
        )
    }

    /// See `InferCtxt::region_constraints_added_in_snapshot`.
    pub(crate) fn region_constraints_added_in_snapshot(&self, mark: &RegionSnapshot) -> bool {
        self.storage.legacy_data.constraints.len() > mark.constraints as usize
    }
}

impl fmt::Debug for RegionSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RegionSnapshot")
    }
}

impl<'db> fmt::Debug for GenericKind<'db> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            GenericKind::Param(ref p) => write!(f, "{p:?}"),
            GenericKind::Placeholder(ref p) => write!(f, "{p:?}"),
            GenericKind::Alias(ref p) => write!(f, "{p:?}"),
        }
    }
}

impl<'db> fmt::Display for GenericKind<'db> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            GenericKind::Param(ref p) => write!(f, "{p:?}"),
            GenericKind::Placeholder(ref p) => write!(f, "{p:?}"),
            GenericKind::Alias(ref p) => write!(f, "{p}"),
        }
    }
}

impl<'db> GenericKind<'db> {
    pub fn to_ty(&self, interner: DbInterner<'db>) -> Ty<'db> {
        match *self {
            GenericKind::Param(ref p) => (*p).to_ty(interner),
            GenericKind::Placeholder(ref p) => Ty::new_placeholder(interner, *p),
            // The next solver only records outlives for aliases it could not normalize.
            GenericKind::Alias(ref p) => (*p).to_ty(interner, IsRigid::Yes),
        }
    }
}

impl<'db> VerifyBound<'db> {
    pub fn must_hold(&self) -> bool {
        match self {
            VerifyBound::IfEq(..) => false,
            VerifyBound::OutlivedBy(re) => re.is_static(),
            VerifyBound::IsEmpty => false,
            VerifyBound::AnyBound(bs) => bs.iter().any(|b| b.must_hold()),
            VerifyBound::AllBounds(bs) => bs.iter().all(|b| b.must_hold()),
        }
    }

    pub fn cannot_hold(&self) -> bool {
        match self {
            VerifyBound::IfEq(..) => false,
            VerifyBound::IsEmpty => false,
            VerifyBound::OutlivedBy(_) => false,
            VerifyBound::AnyBound(bs) => bs.iter().all(|b| b.cannot_hold()),
            VerifyBound::AllBounds(bs) => bs.iter().any(|b| b.cannot_hold()),
        }
    }

    pub fn or(self, vb: VerifyBound<'db>) -> VerifyBound<'db> {
        if self.must_hold() || vb.cannot_hold() {
            self
        } else if self.cannot_hold() || vb.must_hold() {
            vb
        } else {
            // alloc: the verify owns its two bounds.
            VerifyBound::AnyBound(vec![self, vb])
        }
    }
}

impl<'db> RegionConstraintData<'db> {
    /// Returns `true` if this region constraint data contains no constraints, and `false`
    /// otherwise.
    pub fn is_empty(&self) -> bool {
        let RegionConstraintData { constraints, member_constraints, verifys } = self;
        constraints.is_empty() && member_constraints.is_empty() && verifys.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn var(index: u32) -> RegionVid {
        RegionVid::from_u32(index)
    }

    #[test]
    fn columns_round_trip_kind_operands_and_visibility() {
        let mut constraints = RegionConstraints::default();
        let rows = [
            (var(0), var(1), VisibleForLeakCheck::Yes),
            (var(7), var(3), VisibleForLeakCheck::No),
            // `RegionVid`'s largest index.
            (var(0xFFFF_FF00), var(0), VisibleForLeakCheck::Unreachable),
        ];
        for (sub, sup, visible) in rows {
            constraints.push(Constraint::VarSubVar(sub, sup), visible);
        }
        assert_eq!(constraints.len(), 3);
        assert!(constraints.regions.is_empty());
        for (at, (sub, sup, visible)) in rows.into_iter().enumerate() {
            assert_eq!(constraints.get(at), Constraint::VarSubVar(sub, sup));
            assert_eq!(constraints.visible_for_leak_check(at), visible);
        }
        assert_eq!(constraints.iter_from(1).collect::<Vec<_>>(), constraints.to_vec()[1..]);
        constraints.truncate(1, 0);
        assert_eq!(constraints.to_vec(), [Constraint::VarSubVar(var(0), var(1))]);
    }

    #[test]
    fn a_row_is_nine_bytes() {
        let mut constraints = RegionConstraints::default();
        constraints.push(Constraint::VarSubVar(var(0), var(1)), VisibleForLeakCheck::Yes);
        let row = size_of_val(&constraints.tag[0])
            + size_of_val(&constraints.sub[0])
            + size_of_val(&constraints.sup[0]);
        assert_eq!(row, 9);
    }
}
