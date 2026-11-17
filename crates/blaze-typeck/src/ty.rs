//! What the driver asks of the type tables and the trait solver (typeck.md, "`TypeCtx`"). These
//! are the minimal traits until `crates/blaze-ty` exists; each item names what it becomes there.
//! The tests implement them over a toy type language (`crate::toy`).

use crate::error::{CheckError, Stop};
use crate::ids::{ExprId, FloatTy, FnPtrHeader, IntTy, ItemKey, Mutbl, Name, PatId, UintTy};
use crate::world::{Reads, World};

/// A type: a 4-byte handle into the context's tables. Becomes blaze-ty's `Ty`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ty(pub u32);

/// An interned list of types (generic arguments, tuple elements, fn inputs). Becomes blaze-ty's
/// `GenericArgs`; regions are erased in typeck as rustc's typeck sees them, and const arguments
/// are not yet carried (array lengths are values in `TyView::Array`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TyList(pub u32);

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
    /// `Closure(hash(owner, ordinal), [args.., kind, sig, ()])`.
    ClosureOf { owner: ItemKey, ordinal: u32, args: TyList, sig: Ty, kind: ClosureKind },
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
}

/// Where an obligation came from, for its error.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cause {
    Expr(ExprId),
    Pat(PatId),
    Body,
}

/// A goal and its cause.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Obligation {
    pub goal: Goal,
    pub cause: Cause,
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

/// The type tables, the inference table and the solver, as the driver uses them. Becomes the
/// type context of blaze-ty over `rustc_type_ir::Interner` and the next solver's
/// `FulfillmentCtxt`.
pub trait TypeCtx {
    /// A snapshot of the inference table and the pending obligations
    /// (`InferCtxt::start_snapshot`).
    type Snapshot;

    /// The interned type of `v`.
    fn mk(&mut self, v: TyView) -> Result<Ty, CheckError>;
    /// One level of `t`, unresolved: a variable is `Infer` even when bound (see
    /// `shallow_resolve`).
    fn view(&self, t: Ty) -> TyView;
    /// The interned list of `tys`.
    fn mk_list(&mut self, tys: &[Ty]) -> Result<TyList, CheckError>;
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
    /// Makes `a` and `b` equal, binding variables. `Ok(false)`: they cannot be, and nothing was
    /// bound (the context rolls back its own partial work).
    fn unify(&mut self, a: Ty, b: Ty) -> Result<bool, CheckError>;

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
    /// Whether `goal` may hold, evaluated in a probe (`predicate_may_hold`): nothing is bound.
    fn may_hold<W: World>(
        &mut self,
        goal: Goal,
        reads: &mut Reads<'_, '_, W>,
    ) -> Result<bool, Stop>;
    /// `<args[0] as trait_<args[1..]>>::name` normalized now, if one impl or bound selects it;
    /// `None` when ambiguous or unsatisfied. Binds what the selection binds.
    fn normalize<W: World>(
        &mut self,
        trait_: ItemKey,
        args: TyList,
        name: Name,
        reads: &mut Reads<'_, '_, W>,
    ) -> Result<Option<Ty>, Stop>;

    /// The closure-signature hint of `var` from pending obligations
    /// (`deduce_closure_signature`): the inputs and output of the first pending
    /// `var: Fn*<(inputs..)>` obligation (`fn_traits` is `[Fn, FnMut, FnOnce]`): the inputs'
    /// tuple elements, and the term of a pending `<var as FnOnce<..>>::Output` projection
    /// (`output` is its name) if there is one. `None` when no such obligation is pending.
    fn closure_hint(&mut self, var: Ty, fn_traits: [ItemKey; 3], output: Name) -> Option<(TyList, Option<Ty>)>;

    /// Fixes closure `closure`'s kind once its captures are known (rustc's upvar analysis,
    /// `closure_kind`), so its `Fn*` obligations select. `false` when the kind was already
    /// fixed to another.
    fn settle_closure_kind(&mut self, closure: ExprId, kind: ClosureKind) -> Result<bool, CheckError>;
}

/// A closure's kind (`rustc_type_ir::ClosureKind`), weakest trait first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ClosureKind {
    Fn,
    FnMut,
    FnOnce,
}
