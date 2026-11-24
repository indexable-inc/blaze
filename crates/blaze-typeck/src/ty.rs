//! What the driver asks of the type tables and the trait solver (typeck.md, "`TypeCtx`"). These
//! are the minimal traits until `crates/blaze-ty` exists; each item names what it becomes there.
//! The tests implement them over a toy type language (`crate::toy`).

use crate::error::{CheckError, Stop};
use crate::ids::{ExprId, FloatTy, FnPtrHeader, IntTy, ItemKey, Mutbl, Name, PatId, RecordSchema, UintTy};
use crate::world::{Reads, World};

/// A type: a 4-byte handle into the context's tables. Becomes blaze-ty's `Ty`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ty(pub u32);

/// An interned list of types (generic arguments, tuple elements, fn inputs). Becomes blaze-ty's
/// `GenericArgs`; regions are erased in typeck as rustc's typeck sees them, and const arguments
/// are not yet carried (array lengths are values in `TyView::Array`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TyList(pub u32);

/// The interned flags relevant to projection normalization, before inference bindings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TyFlags(u16);

impl TyFlags {
    pub const HAS_ALIAS: u16 = blaze_ty::Flags::ALIAS;
    /// Type, integer, float and const variables. Typeck erases regions.
    pub const HAS_INFER: u16 = blaze_ty::Flags::TY_INFER | blaze_ty::Flags::CT_INFER;

    pub(crate) const fn from_interned(flags: blaze_ty::Flags) -> Self {
        Self(flags.bits() & (Self::HAS_ALIAS | Self::HAS_INFER))
    }

    pub const fn has(self, mask: u16) -> bool {
        self.0 & mask != 0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// What kind of inference variable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VarKind {
    /// Any type.
    Ty,
    /// An integer type (from an unsuffixed integer literal).
    Int,
    /// A float type (from an unsuffixed float literal).
    Float,
    /// A const generic argument not yet known (`f::<_>()`, an array length to infer).
    Const,
}

/// An inference variable: its kind and its index in the context.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Var {
    pub kind: VarKind,
    pub index: u32,
}

/// One level of a type, its children by handle. Becomes `rustc_type_ir::TyKind` through
/// blaze-ty's interner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TyView {
    Bool,
    Char,
    Int(IntTy),
    Uint(UintTy),
    Float(FloatTy),
    Str,
    Never,
    Tuple(TyList),
    Record(RecordSchema, TyList),
    Union(TyList),
    Ref(Mutbl, Ty),
    Ptr(Mutbl, Ty),
    /// `[elem; len]`: `len` a const term of type `usize` (`ConstValue`, `ConstParam`,
    /// `ConstInfer`).
    Array(Ty, Ty),
    Slice(Ty),
    /// A pattern type `base is PAT`: `PNode::Pat`'s view, its ranges `ConstValue` terms in
    /// pairs. Opaque to the checker: no coercion, deref, field or method reaches the base.
    Pat(Ty, TyList),
    Adt(ItemKey, TyList),
    FnDef(ItemKey, TyList),
    /// Inputs, then the output last; its ABI and safety.
    FnPtr(TyList, FnPtrHeader),
    /// A closure of this body and its signature (a `FnPtr`).
    Closure(ExprId, Ty),
    /// A closure of another body (`PNode::Closure`): its identity (`owner`, `ordinal`, the
    /// owner's arguments `args`) and the facts the owner's check recorded (`Question::Closure`):
    /// its signature (a `FnPtr`, substituted by `args`) and its kind. The solver sees
    /// `Closure(hash(owner, ordinal), [args.., kind, sig, environment])`.
    ClosureOf { owner: ItemKey, ordinal: u32, args: TyList, sig: Ty, kind: ClosureKind, upvars: Option<Ty> },
    /// An async block's future of another body (`PNode::Coroutine`): its identity (`owner`,
    /// `ordinal`, the owner's arguments `args`) and the facts the owner's check recorded
    /// (`Question::Coroutine`): its output and interior, substituted by `args`. The solver sees
    /// `Coroutine(coroutine_def(owner, ordinal), [output, interior])`, so two bodies agree.
    CoroutineOf { owner: ItemKey, ordinal: u32, args: TyList, output: Ty, interior: Ty },
    /// An async block's (or an async closure's call's) future, by its expression: its output,
    /// then its interior, the tuple of what it holds (its captures by capture kind and the
    /// values live across an `.await`): a variable until the end sequence fixes it.
    /// `Future<Output = output>`; its auto traits are its interior's; never `Unpin`.
    Future(ExprId, Ty, Ty),
    Param(u32),
    /// `<args[0] as trait_<args[1..]>>::name`, not yet normalized.
    Alias { trait_: ItemKey, name: Name, args: TyList },
    Opaque(ItemKey, TyList),
    /// A type variable (`kind` `Ty`, `Int` or `Float`); never a `Const` variable.
    Infer(Var),
    Error,
    // Const terms: a handle is any generic argument, so an array length or a const generic
    // argument is a `Ty` handle whose view is one of these (typeck.md, "Const generics").
    /// A value of a const generic: its type (an integer, `char` or `bool`) and its bits,
    /// little-endian as consteval writes them.
    ConstValue { ty: Ty, bits: u128 },
    /// A const generic parameter, by its index in the item's generics (as `Param`).
    ConstParam(u32),
    /// A const variable (`kind` is `VarKind::Const`).
    ConstInfer(Var),
    /// A trait object: `PNode::Dynamic`'s view, its predicates in that one order (made by
    /// `TypeCtx::mk` from a `PNode::Dynamic`, never reordered here).
    Dynamic(TyList),
    /// The predicate rows of a `Dynamic` (`PNode::ExTrait`, `ExProj`, `ExAuto`): not types.
    ExTrait(ItemKey, TyList),
    /// `name = term` of `trait_`: `args_term` is the trait's args after `Self`, then the term
    /// last (blaze-ty's `ex_projection` row, read without interning).
    ExProj { trait_: ItemKey, name: Name, args_term: TyList },
    ExAuto(ItemKey),
    /// An extern type (`PNode::Foreign`'s view; blaze-ty's `TyKind::Foreign`): itself only, no
    /// size, no fields, no deref, no method but its inherent impls' and its traits'.
    Foreign(ItemKey),
}

impl TyView {
    /// Whether this is a const term, not a type.
    pub fn is_const(&self) -> bool {
        matches!(self, TyView::ConstValue { .. } | TyView::ConstParam(_) | TyView::ConstInfer(_))
    }
}

/// What an obligation asks. `args[0]` is the self type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Goal {
    /// `args[0]: trait_<args[1..]>`.
    Trait { trait_: ItemKey, args: TyList },
    /// `<args[0] as trait_<args[1..]>>::name == term`.
    Projection { trait_: ItemKey, args: TyList, name: Name, term: Ty },
    /// `ty` is well formed.
    WellFormed(Ty),
    /// The `where const` condition `cond` holds at `args` (`PClause::Const`).
    ConstHolds { cond: ItemKey, args: TyList },
    /// `for<F in fields(container)> F: trait_<args...>` (`PClause::ForFields`; `args` = args after `Self`).
    ForFields { container: Ty, trait_: ItemKey, args: TyList },
}

/// Where an obligation came from, for its error.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cause {
    Expr(ExprId),
    Pat(PatId),
    Body,
    /// The `nth` where-clause of the item the path or method call `expr` names: filed, when
    /// unmet, at the text that introduced it (rules/obligation_site.rs), else at `expr`.
    Clause { expr: ExprId, nth: u32 },
    /// Text the user wrote that is no expression (a qualified path's anchor type), by its range.
    Written(crate::ids::Range),
}

/// A goal and its cause.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Obligation {
    pub goal: Goal,
    pub cause: Cause,
}

/// A projection normalized now ([`TypeCtx::normalize`]): its type, and whether the selection
/// that gave it is certain or still holds an undecided nested goal (an impl's where-clause on
/// a closure whose kind is not settled yet: `impl<T, F> Deref for ScopeGuard<T, F> where F:
/// FnMut(&mut T)`, hashbrown 0.14's src/scopeguard.rs). An ambiguous answer's type is still
/// the answer (rustc's new solver applies an ambiguous response's constraints, rustc_next_trait_solver
/// `instantiate_and_apply_query_response`); its caller keeps the projection as an obligation
/// so the nested goal is proved once it can be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Normalized {
    Certain(Ty),
    Ambiguous(Ty),
}

/// How an obligation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FulfillErrorKind {
    /// No impl or bound can hold it.
    Unsatisfied,
    /// Still ambiguous at `select_all`: "type annotations needed".
    Ambiguous,
    /// A projection's normalized type does not equal its term.
    Mismatch,
}

/// An obligation that failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FulfillError {
    pub obligation: Obligation,
    pub kind: FulfillErrorKind,
    /// An unmet trait bound's reason, when it is an impl's `where const` condition.
    pub because: Option<crate::diag::Because>,
}

/// How many inference variables of each kind (type, integer, float, const, in that order)
/// existed at one moment ([`TypeCtx::var_mark`]): a variable whose index is at or past its
/// kind's count was made after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VarMark(pub [u32; 4]);

/// The type tables, the inference table and the solver, as the driver uses them. Becomes the
/// type context of blaze-ty over `rustc_type_ir::Interner` and the next solver's
/// `FulfillmentCtxt`.
pub trait TypeCtx {
    /// A snapshot of the inference table and the pending obligations
    /// (`InferCtxt::start_snapshot`).
    type Snapshot;

    /// The interned type of `v`.
    fn mk(&mut self, v: TyView) -> Result<Ty, CheckError>;
    /// The flags of the interned node, including every child's flags.
    fn ty_flags(&self, t: Ty) -> TyFlags;
    /// One level of `t`, unresolved: a variable is `Infer` even when bound (see
    /// `shallow_resolve`). A handle the context cannot view is `Error`, noted for
    /// [`TypeCtx::take_malformed`].
    fn view(&self, t: Ty) -> TyView;
    /// The first handle a read that returns no error (`view`, `list_get`, `list_len`,
    /// `shallow_resolve`) could not read since the last call, taken: the check of a body fails
    /// with it when it ends (lib.rs `run`; rule 20: a malformed input fails its own body).
    fn take_malformed(&mut self) -> Option<crate::error::Malformed>;
    /// The interned list of `tys`.
    fn mk_list(&mut self, tys: &[Ty]) -> Result<TyList, CheckError>;
    /// The union of the elements' flags, recorded when the list was interned.
    fn list_flags(&self, l: TyList) -> TyFlags;
    /// The length of `l`.
    fn list_len(&self, l: TyList) -> u32;
    /// The `i`th type of `l`; `i < list_len(l)`.
    fn list_get(&self, l: TyList, i: u32) -> Ty;

    /// A new variable of `kind`.
    fn fresh(&mut self, kind: VarKind) -> Result<Ty, CheckError>;
    /// `t`, with a bound variable at its top replaced by its value, repeatedly.
    fn shallow_resolve(&mut self, t: Ty) -> Ty;
    /// `t` with every bound variable inside it replaced by its value. Unbound ones stay.
    fn resolve(&mut self, t: Ty) -> Result<Ty, CheckError>;
    /// Whether a type of `l`, resolved, still holds an inference variable.
    fn list_has_infer(&mut self, l: TyList) -> bool;
    /// How many variables of each kind exist now: the mark [`TypeCtx::list_has_infer_since`]
    /// compares with.
    fn var_mark(&self) -> VarMark;
    /// Whether a type of `l`, resolved, holds a type, integer, float or const variable made
    /// after `mark` was taken (rustc's `fudge_inference_if_ok`: a variable made inside a probe
    /// is gone after its rollback; one made before is the same variable after it). Region
    /// variables are not counted, as `list_has_infer` does not count them.
    fn list_has_infer_since(&mut self, l: TyList, mark: VarMark) -> bool;
    /// Makes `a` and `b` equal, binding variables. `Ok(false)`: they cannot be, and nothing was
    /// bound (the context rolls back its own partial work).
    fn unify(&mut self, a: Ty, b: Ty) -> Result<bool, CheckError>;
    /// [`TypeCtx::unify`], where each projection goal the relation defers (an alias met by
    /// another type, `<X as Tr>::A == u16`) is an obligation of `cause`: when it later fails,
    /// the finding is at the place whose check related the two types, never at no place
    /// (error.rs `DiagWithoutPlace`). `unify` is this at `Cause::Body`, for relations no
    /// finding comes from (a probe rolled back, an instance's selection).
    fn unify_at(&mut self, a: Ty, b: Ty, cause: Cause) -> Result<bool, CheckError>;

    /// Starts a snapshot.
    fn snapshot(&mut self) -> Self::Snapshot;
    /// Undoes everything since `s`, obligations registered included.
    fn rollback_to(&mut self, s: Self::Snapshot);
    /// Keeps everything since `s`.
    fn commit(&mut self, s: Self::Snapshot);

    /// Adds a where-clause of the body's item to the param env.
    fn assume(&mut self, clause: Goal) -> Result<(), CheckError>;
    /// Registers an obligation to be proved.
    fn register(&mut self, ob: Obligation) -> Result<(), CheckError>;
    /// Proves what can be proved now; failures go to `next_error`, ambiguous ones stay.
    fn select_where_possible<W: World>(
        &mut self,
        reads: &mut Reads<'_, '_, W>,
    ) -> Result<(), Stop>;
    /// As `select_where_possible`, then every obligation left is an `Ambiguous` error.
    fn select_all<W: World>(
        &mut self,
        reads: &mut Reads<'_, '_, W>,
    ) -> Result<(), Stop>;
    /// The next failed obligation, in the order they failed.
    fn next_error(&mut self) -> Option<FulfillError>;
    /// Appends the goal of every obligation still pending (neither proved nor failed), in the
    /// order registered: what an E0282 names as waiting on its variable (rules/writeback.rs
    /// `undecided_of`).
    fn pending_goals(&self, out: &mut blaze_own::Vec<Goal, blaze_own::Alloc<'_>>);
    /// Whether `goal` may hold, evaluated in a probe (`predicate_may_hold`): nothing is bound.
    fn may_hold<W: World>(
        &mut self,
        goal: Goal,
        reads: &mut Reads<'_, '_, W>,
    ) -> Result<bool, Stop>;
    /// Why `goal` has no solution (blaze-solve `Solver::why_no`), in a probe: nothing is bound.
    /// The steps from `goal` down to the first goal under it that fails by itself are appended
    /// to `steps`, `goal` first ([`crate::diag::WhyReached::Root`]), and why that one fails is
    /// the answer; `None` (and no step) when the goal holds or is ambiguous under the walk. A
    /// step's goal is kept only when it is a positive trait goal naming no variable the walk
    /// made (each dies with the probe).
    fn why_chain<W: World>(
        &mut self,
        goal: Goal,
        steps: &mut blaze_own::Vec<crate::diag::PendingWhyStep, blaze_own::Alloc<'_>>,
        reads: &mut Reads<'_, '_, W>,
    ) -> Result<Option<crate::diag::WhyLeaf>, Stop>;
    /// Why `goal` is still undecided (blaze-solve `Solver::why_maybe`), in a probe: nothing is
    /// bound. The steps from `goal` down to the first goal under it that is undecided by itself
    /// are appended to `steps` as [`TypeCtx::why_chain`]'s, and why that one is undecided is
    /// the answer (an undecided goal's [`crate::diag::WhyLeaf`]s); `None` (and no step) when the
    /// goal is decided under the walk. A step's goal is kept only when it is a positive trait
    /// goal naming no inference variable at all (the record holds none).
    fn why_maybe_chain<W: World>(
        &mut self,
        goal: Goal,
        steps: &mut blaze_own::Vec<crate::diag::PendingWhyStep, blaze_own::Alloc<'_>>,
        reads: &mut Reads<'_, '_, W>,
    ) -> Result<Option<crate::diag::WhyLeaf>, Stop>;
    /// The typed cause of an undecided `goal` that stalled on an opaque type: the first opaque
    /// its evaluation read whose hidden type the world does not give this body, `skip`'s aside
    /// (the body's own, defined here). One ordinary evaluation in a probe, nothing bound, never
    /// the diagnostic walk; `None` when the goal is decided or read no such opaque. The asker
    /// reveals it (front2 opaque_auto.rs `after_auto_gap`) and asks again: auto traits leak
    /// through `impl Trait` at any depth (the Reference, "Auto trait leakage").
    fn hidden_unknown<W: World>(
        &mut self,
        goal: Goal,
        skip: &dyn Fn(ItemKey) -> bool,
        reads: &mut Reads<'_, '_, W>,
    ) -> Result<Option<ItemKey>, Stop>;
    /// The goal [`TypeCtx::why_chain`] ends at, where the chain has a step under `goal` and that
    /// step's goal is kept: the where-clause a person fixes (`Rc<u8>: Send` under
    /// `spawn(async { .. })`'s `F: Send`). `None` otherwise: the goal itself is what to fix.
    fn why_leaf<W: World>(
        &mut self,
        goal: Goal,
        reads: &mut Reads<'_, '_, W>,
    ) -> Result<Option<(ItemKey, TyList)>, Stop>;
    /// `<args[0] as trait_<args[1..]>>::name` normalized now, if one impl or bound selects it
    /// ([`Normalized::Certain`]), or selects it with a nested goal still undecided and the type
    /// decided all the same ([`Normalized::Ambiguous`]); `None` when unsatisfied, when the
    /// type is not decided, or on overflow. Binds what the selection binds.
    fn normalize<W: World>(
        &mut self,
        trait_: ItemKey,
        args: TyList,
        name: Name,
        reads: &mut Reads<'_, '_, W>,
    ) -> Result<Option<Normalized>, Stop>;

    /// The closure-signature hint of `var` from pending obligations
    /// (`deduce_closure_signature`): the inputs and output of the first pending
    /// `var: Fn*<(inputs..)>` obligation (`fn_traits` is `[Fn, FnMut, FnOnce]`): the inputs'
    /// tuple elements, and the term of a pending `<var as FnOnce<..>>::Output` projection
    /// (`output` is its name) whose `Args` can equal the inputs' tuple (a probe: each clause of
    /// a bound is lowered on its own, so an alias in `Args` is a different normalization
    /// variable in each), each with the cause of the obligation it came from (where the
    /// expectation came from: D148, `columns::SigFrom`). `None` when no such obligation is
    /// pending.
    fn closure_hint(&mut self, var: Ty, fn_traits: [ItemKey; 3], output: Name) -> Result<Option<((TyList, Cause), Option<(Ty, Cause)>)>, CheckError>;

    /// Supply the inferred call kind and the stored capture types. A missing tuple
    /// means the binding rows did not establish the precise captured places.
    fn settle_closure(&mut self, closure: ExprId, kind: ClosureKind, upvars: Option<Ty>) -> Result<(), CheckError>;
}

/// A closure's kind (`rustc_type_ir::ClosureKind`), weakest trait first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ClosureKind {
    Fn,
    FnMut,
    FnOnce,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InferCtx;
    use blaze_own::Alloc;

    #[test]
    fn deep_concrete_type_has_empty_flags() {
        let mut cx = InferCtx::new_in(Alloc::global());
        let mut ty = cx.mk(TyView::Bool).unwrap();
        for _ in 0..256 {
            let pair = cx.mk_list(&[ty, ty]).unwrap();
            ty = cx.mk(TyView::Tuple(pair)).unwrap();
        }
        let list = cx.mk_list(&[ty]).unwrap();
        assert!(cx.list_flags(list).is_empty());
        assert!(cx.ty_flags(ty).is_empty());
    }

    #[test]
    fn projection_three_levels_deep_sets_alias_flag() {
        let mut cx = InferCtx::new_in(Alloc::global());
        let param = cx.mk(TyView::Param(0)).unwrap();
        let args = cx.mk_list(&[param]).unwrap();
        let projection = cx.mk(TyView::Alias { trait_: ItemKey(1), name: Name::of("Output"), args }).unwrap();
        let slice = cx.mk(TyView::Slice(projection)).unwrap();
        let reference = cx.mk(TyView::Ref(Mutbl::Not, slice)).unwrap();
        let pair = cx.mk_list(&[param, reference]).unwrap();
        let root = cx.mk(TyView::Tuple(pair)).unwrap();
        assert!(cx.list_flags(pair).has(TyFlags::HAS_ALIAS));
        assert!(cx.ty_flags(root).has(TyFlags::HAS_ALIAS));
        assert!(!cx.ty_flags(root).has(TyFlags::HAS_INFER));
    }

    #[test]
    fn type_and_const_variables_each_set_the_infer_flag() {
        let mut cx = InferCtx::new_in(Alloc::global());
        for kind in [VarKind::Ty, VarKind::Int, VarKind::Float, VarKind::Const] {
            let var = cx.fresh(kind).unwrap();
            let args = cx.mk_list(&[var]).unwrap();
            let root = cx.mk(TyView::Adt(ItemKey(1), args)).unwrap();
            assert!(cx.list_flags(args).has(TyFlags::HAS_INFER));
            assert!(cx.ty_flags(root).has(TyFlags::HAS_INFER));
            assert!(!cx.ty_flags(root).has(TyFlags::HAS_ALIAS));
        }
    }
}
