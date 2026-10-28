//! MIR definitions and implementation

use std::{fmt::Display, iter};

use base_db::target::TargetLoadError;
use either::Either;
use hir_def::{
    ExpressionStoreOwnerId, FieldId, FunctionId, GenericParamId, LocalFieldId, StaticId, TraitId,
    UnionId, VariantId,
    expr_store::{ExpressionStore, path::Path},
    hir::{BindingId, ClosureKind, CoroutineSource, Expr, ExprId, Ordering, PatId},
};
use hir_expand::name::Name;
use la_arena::{Arena, ArenaMap, Idx, RawIdx};
use macros::{GenericTypeVisitable, TypeFoldable, TypeVisitable};
use rustc_hash::FxHashMap;
use rustc_type_ir::{
    CoroutineArgs, GenericTypeVisitable,
    inherent::{GenericArgs as _, IntoKind, Ty as _},
};
use salsa::SalsaValue;
use smallvec::{SmallVec, smallvec};
use stdx::impl_from;

use crate::{
    CallableDefId, InferBodyId, InferenceResult, MemoryMap,
    consteval::ConstEvalError,
    db::{HirDatabase, InternedClosure, InternedClosureId, InternedCoroutineId},
    infer::PointerCast,
    layout::LayoutError,
    next_solver::{
        Allocation, AllocationData, DbInterner, ErrorGuaranteed, GenericArgs, ParamEnv,
        StoredAllocation, StoredConst, StoredGenericArgs, StoredTy, Ty, TyKind,
        infer::{InferCtxt, traits::ObligationCause},
        interned_slice,
        obligation_ctxt::ObligationCtxt,
    },
};

mod held;
pub mod target;

pub use held::{HeldLocals, YieldPoint};

pub type BasicBlockId = Idx<BasicBlock>;
pub type LocalId = Idx<Local>;

pub fn return_slot() -> LocalId {
    LocalId::from_raw(RawIdx::from(0))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Local {
    pub ty: StoredTy,
}

/// An operand in MIR represents a "value" in Rust, the definition of which is undecided and part of
/// the memory model. One proposal for a definition of values can be found [on UCG][value-def].
///
/// [value-def]: https://github.com/rust-lang/unsafe-code-guidelines/blob/master/wip/value-domain.md
///
/// The most common way to create values is via loading a place. Loading a place is an operation
/// which reads the memory of the place and converts it to a value. This is a fundamentally *typed*
/// operation. The nature of the value produced depends on the type of the conversion. Furthermore,
/// there may be other effects: if the type has a validity constraint loading the place might be UB
/// if the validity constraint is not met.
///
/// **Needs clarification:** Ralf proposes that loading a place not have side-effects.
/// This is what is implemented in miri today. Are these the semantics we want for MIR? Is this
/// something we can even decide without knowing more about Rust's memory model?
///
/// **Needs clarification:** Is loading a place that has its variant index set well-formed? Miri
/// currently implements it, but it seems like this may be something to check against in the
/// validator.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Operand {
    pub kind: OperandKind,
    // FIXME : This should actually just be of type `MirSpan`.
    pub span: Option<MirSpan>,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum OperandKind {
    /// Creates a value by loading the given place.
    ///
    /// Before drop elaboration, the type of the place must be `Copy`. After drop elaboration there
    /// is no such requirement.
    Copy(StoredPlace),

    /// Creates a value by performing loading the place, just like the `Copy` operand.
    ///
    /// This *may* additionally overwrite the place with `uninit` bytes, depending on how we decide
    /// in [UCG#188]. You should not emit MIR that may attempt a subsequent second load of this
    /// place without first re-initializing it.
    ///
    /// [UCG#188]: https://github.com/rust-lang/unsafe-code-guidelines/issues/188
    Move(StoredPlace),
    /// Constants are already semantically values, and remain unchanged.
    Constant {
        konst: StoredConst,
        ty: StoredTy,
    },
    Allocation {
        allocation: StoredAllocation,
    },
    /// NON STANDARD: This kind of operand returns an immutable reference to that static memory. Rustc
    /// handles it with the `Constant` variant somehow.
    Static(StaticId),
}

impl<'db> Operand {
    pub fn from_concrete_const(data: Box<[u8]>, memory_map: MemoryMap<'db>, ty: Ty<'db>) -> Self {
        Operand {
            kind: OperandKind::Allocation {
                allocation: Allocation::new(AllocationData { ty, memory: data, memory_map })
                    .store(),
            },
            span: None,
        }
    }

    pub fn from_bytes(data: Box<[u8]>, ty: Ty<'db>) -> Self {
        Operand::from_concrete_const(data, MemoryMap::default(), ty)
    }

    pub fn const_zst(ty: Ty<'db>) -> Operand {
        Self::from_bytes(Box::default(), ty)
    }

    pub fn from_fn(
        db: &'db dyn HirDatabase,
        func_id: hir_def::FunctionId,
        generic_args: GenericArgs<'db>,
    ) -> Operand {
        let interner = DbInterner::new_no_crate(db);
        let ty = Ty::new_fn_def(interner, CallableDefId::FunctionId(func_id).into(), generic_args);
        Operand::from_bytes(Box::default(), ty)
    }

    /// The expression the operand was written as, where lowering kept one.
    pub fn span(&self) -> Option<MirSpan> {
        self.span
    }
}

/// The index of a field (whether of a struct/enum variant, tuple, or closure).
/// For a struct/enum it converts from and to the LocalFieldId, for a tuple or closure it's simply the index.
#[derive(Copy, Clone, PartialEq, Eq, Hash, salsa::SalsaValue, PartialOrd, Ord, Debug)]
pub struct FieldIndex(pub u32);

impl FieldIndex {
    pub fn to_local_field_id(self) -> LocalFieldId {
        LocalFieldId::from_raw(RawIdx::from_u32(self.0))
    }
}

impl From<LocalFieldId> for FieldIndex {
    fn from(value: LocalFieldId) -> Self {
        FieldIndex(value.into_raw().into_u32())
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum ProjectionElem<V: PartialEq> {
    Deref,
    /// A field (e.g., `f` in `_1.f`).
    Field(FieldIndex),
    /// Index into a slice/array.
    Index(V),
    /// These indices are generated by slice patterns. With `from_end`, `offset` counts from the
    /// end and 1 is the last element, as in rustc's MIR. `min_length` is the length the pattern
    /// checked the slice has at least; the borrow checker needs it to tell from-start and
    /// from-end indices apart.
    ConstantIndex {
        offset: u64,
        min_length: u64,
        from_end: bool,
    },
    /// These indices are generated by slice patterns.
    Subslice {
        from: u64,
        to: u64,
    },
    /// "Downcast" to a variant of an enum or a coroutine.
    Downcast(VariantId),
}

impl<V: PartialEq> ProjectionElem<V> {
    pub fn map<V2: PartialEq>(self, v: impl FnOnce(V) -> V2) -> ProjectionElem<V2> {
        match self {
            ProjectionElem::Deref => ProjectionElem::Deref,
            ProjectionElem::Field(field_index) => ProjectionElem::Field(field_index),
            ProjectionElem::Index(idx) => ProjectionElem::Index(v(idx)),
            ProjectionElem::ConstantIndex { offset, min_length, from_end } => {
                ProjectionElem::ConstantIndex { offset, min_length, from_end }
            }
            ProjectionElem::Subslice { from, to } => ProjectionElem::Subslice { from, to },
            ProjectionElem::Downcast(variant_id) => ProjectionElem::Downcast(variant_id),
        }
    }

    pub fn try_map<V2: PartialEq>(
        self,
        v: impl FnOnce(V) -> Option<V2>,
    ) -> Option<ProjectionElem<V2>> {
        Some(match self {
            ProjectionElem::Deref => ProjectionElem::Deref,
            ProjectionElem::Field(field_index) => ProjectionElem::Field(field_index),
            ProjectionElem::Index(idx) => ProjectionElem::Index(v(idx)?),
            ProjectionElem::ConstantIndex { offset, min_length, from_end } => {
                ProjectionElem::ConstantIndex { offset, min_length, from_end }
            }
            ProjectionElem::Subslice { from, to } => ProjectionElem::Subslice { from, to },
            ProjectionElem::Downcast(variant_id) => ProjectionElem::Downcast(variant_id),
        })
    }
}

pub type PlaceElem = ProjectionElem<LocalId>;

impl<W: crate::next_solver::WorldExposer> GenericTypeVisitable<W> for PlaceElem {
    fn generic_visit_with(&self, _: &mut W) {}
}

interned_slice!(ProjectionStorage, Projection, StoredProjection, projection, PlaceElem, PlaceElem);

impl<'db> Projection<'db> {
    pub fn project(self, projection: PlaceElem) -> Projection<'db> {
        Projection::new_from_iter(self.as_slice().iter().copied().chain([projection]))
    }
}

impl StoredProjection {
    pub fn as_slice(&self) -> &[PlaceElem] {
        self.as_ref().as_slice()
    }

    pub fn is_empty(&self) -> bool {
        self.as_slice().is_empty()
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct Place<'db> {
    pub local: LocalId,
    pub projection: Projection<'db>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StoredPlace {
    pub local: LocalId,
    pub projection: StoredProjection,
}

impl StoredPlace {
    pub fn as_ref<'db>(&self) -> Place<'db> {
        Place { local: self.local, projection: self.projection.as_ref() }
    }
}

impl<'db> Place<'db> {
    pub fn project(&self, projection: PlaceElem) -> Place<'db> {
        Place { local: self.local, projection: self.projection.project(projection) }
    }

    pub fn store(&self) -> StoredPlace {
        StoredPlace { local: self.local, projection: self.projection.store() }
    }
    pub fn ty(
        &self,
        body: &MirBody<'db>,
        infcx: &InferCtxt<'db>,
        env: ParamEnv<'db>,
    ) -> PlaceTy<'db> {
        PlaceTy::from_ty(body.locals[self.local].ty.as_ref()).multi_projection_ty(
            infcx,
            env,
            self.projection.as_slice(),
        )
    }
}

impl<'db> From<LocalId> for Place<'db> {
    fn from(local: LocalId) -> Self {
        let empty: &[PlaceElem] = &[];
        Place { local, projection: Projection::new_from_slice(empty) }
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum AggregateKind {
    /// The type is of the element
    Array(StoredTy),
    /// The type is of the tuple
    Tuple(StoredTy),
    Adt(VariantId, StoredGenericArgs),
    Union(UnionId, FieldId),
    Closure(StoredTy),
    //Coroutine(LocalDefId, SubstsRef, Movability),
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct SwitchTargets {
    /// Possible values. The locations to branch to in each case
    /// are found in the corresponding indices from the `targets` vector.
    pub values: SmallVec<[u128; 1]>,

    /// Possible branch sites. The last element of this vector is used
    /// for the otherwise branch, so targets.len() == values.len() + 1
    /// should hold.
    //
    // This invariant is quite non-obvious and also could be improved.
    // One way to make this invariant is to have something like this instead:
    //
    // branches: Vec<(ConstInt, BasicBlock)>,
    // otherwise: Option<BasicBlock> // exhaustive if None
    //
    // However we’ve decided to keep this as-is until we figure a case
    // where some other approach seems to be strictly better than other.
    pub targets: SmallVec<[BasicBlockId; 2]>,
}

impl SwitchTargets {
    /// Creates switch targets from an iterator of values and target blocks.
    ///
    /// The iterator may be empty, in which case the `SwitchInt` instruction is equivalent to
    /// `goto otherwise;`.
    pub fn new(
        targets: impl Iterator<Item = (u128, BasicBlockId)>,
        otherwise: BasicBlockId,
    ) -> Self {
        let (values, mut targets): (SmallVec<_>, SmallVec<_>) = targets.unzip();
        targets.push(otherwise);
        Self { values, targets }
    }

    /// Builds a switch targets definition that jumps to `then` if the tested value equals `value`,
    /// and to `else_` if not.
    pub fn static_if(value: u128, then: BasicBlockId, else_: BasicBlockId) -> Self {
        Self { values: smallvec![value], targets: smallvec![then, else_] }
    }

    /// Returns the fallback target that is jumped to when none of the values match the operand.
    pub fn otherwise(&self) -> BasicBlockId {
        *self.targets.last().unwrap()
    }

    /// Returns an iterator over the switch targets.
    ///
    /// The iterator will yield tuples containing the value and corresponding target to jump to, not
    /// including the `otherwise` fallback target.
    ///
    /// Note that this may yield 0 elements. Only the `otherwise` branch is mandatory.
    pub fn iter(&self) -> impl Iterator<Item = (u128, BasicBlockId)> + '_ {
        iter::zip(&self.values, &self.targets).map(|(x, y)| (*x, *y))
    }

    /// Returns a slice with all possible jump targets (including the fallback target).
    pub fn all_targets(&self) -> &[BasicBlockId] {
        &self.targets
    }

    /// Finds the `BasicBlock` to which this `SwitchInt` will branch given the
    /// specific value. This cannot fail, as it'll return the `otherwise`
    /// branch if there's not a specific match for the value.
    pub fn target_for_value(&self, value: u128) -> BasicBlockId {
        self.iter().find_map(|(v, t)| (v == value).then_some(t)).unwrap_or_else(|| self.otherwise())
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Terminator {
    pub span: MirSpan,
    pub kind: TerminatorKind,
}

/// What a failed [`TerminatorKind::Assert`] panics with: rustc's `AssertKind`.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum AssertKind {
    BoundsCheck { len: Operand, index: Operand },
    Overflow(BinOp, Operand, Operand),
    OverflowNeg(Operand),
    DivisionByZero(Operand),
    RemainderByZero(Operand),
}

impl AssertKind {
    /// A check a crate that compiles without overflow checks does not make, as rustc's
    /// `is_optional_overflow_check`: the body has it whatever the crate does.
    pub fn is_optional_overflow_check(&self) -> bool {
        matches!(
            self,
            AssertKind::OverflowNeg(_)
                | AssertKind::Overflow(
                    BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Shl | BinOp::Shr,
                    ..
                )
        )
    }

    pub(crate) fn for_each_operand_mut(&mut self, f: &mut impl FnMut(&mut Operand)) {
        match self {
            AssertKind::BoundsCheck { len, index } => {
                f(len);
                f(index);
            }
            AssertKind::Overflow(_, a, b) => {
                f(a);
                f(b);
            }
            AssertKind::OverflowNeg(a)
            | AssertKind::DivisionByZero(a)
            | AssertKind::RemainderByZero(a) => f(a),
        }
    }

    pub fn for_each_operand(&self, f: &mut impl FnMut(&Operand)) {
        match self {
            AssertKind::BoundsCheck { len, index } => {
                f(len);
                f(index);
            }
            AssertKind::Overflow(_, a, b) => {
                f(a);
                f(b);
            }
            AssertKind::OverflowNeg(a)
            | AssertKind::DivisionByZero(a)
            | AssertKind::RemainderByZero(a) => f(a),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum TerminatorKind {
    /// Block has one successor; we continue execution there.
    Goto { target: BasicBlockId },

    /// Switches based on the computed value.
    ///
    /// First, evaluates the `discr` operand. The type of the operand must be a signed or unsigned
    /// integer, char, or bool, and must match the given type. Then, if the list of switch targets
    /// contains the computed value, continues execution at the associated basic block. Otherwise,
    /// continues execution at the "otherwise" basic block.
    ///
    /// Target values may not appear more than once.
    SwitchInt {
        /// The discriminant value being tested.
        discr: Operand,

        targets: SwitchTargets,
    },

    /// Indicates that the landing pad is finished and that the process should continue unwinding.
    ///
    /// Like a return, this marks the end of this invocation of the function.
    ///
    /// Only permitted in cleanup blocks. `Resume` is not permitted with `-C unwind=abort` after
    /// deaggregation runs.
    UnwindResume,

    /// Indicates that the landing pad is finished and that the process should abort.
    ///
    /// Used to prevent unwinding for foreign items or with `-C unwind=abort`. Only permitted in
    /// cleanup blocks.
    Abort,

    /// Returns from the function.
    ///
    /// Like function calls, the exact semantics of returns in Rust are unclear. Returning very
    /// likely at least assigns the value currently in the return place (`_0`) to the place
    /// specified in the associated `Call` terminator in the calling function, as if assigned via
    /// `dest = move _0`. It might additionally do other things, like have side-effects in the
    /// aliasing model.
    ///
    /// If the body is a coroutine body, this has slightly different semantics; it instead causes a
    /// `CoroutineState::Returned(_0)` to be created (as if by an `Aggregate` rvalue) and assigned
    /// to the return place.
    Return,

    /// Indicates a terminator that can never be reached.
    ///
    /// Executing this terminator is UB.
    Unreachable,

    /// The behavior of this statement differs significantly before and after drop elaboration.
    /// After drop elaboration, `Drop` executes the drop glue for the specified place, after which
    /// it continues execution/unwinds at the given basic blocks. It is possible that executing drop
    /// glue is special - this would be part of Rust's memory model. (**FIXME**: due we have an
    /// issue tracking if drop glue has any interesting semantics in addition to those of a function
    /// call?)
    ///
    /// `Drop` before drop elaboration is a *conditional* execution of the drop glue. Specifically, the
    /// `Drop` will be executed if...
    ///
    /// **Needs clarification**: End of that sentence. This in effect should document the exact
    /// behavior of drop elaboration. The following sounds vaguely right, but I'm not quite sure:
    ///
    /// > The drop glue is executed if, among all statements executed within this `Body`, an assignment to
    /// > the place or one of its "parents" occurred more recently than a move out of it. This does not
    /// > consider indirect assignments.
    Drop { place: StoredPlace, target: BasicBlockId, unwind: Option<BasicBlockId> },

    /// Drops the place and assigns a new value to it.
    ///
    /// This first performs the exact same operation as the pre drop-elaboration `Drop` terminator;
    /// it then additionally assigns the `value` to the `place` as if by an assignment statement.
    /// This assignment occurs both in the unwind and the regular code paths. The semantics are best
    /// explained by the elaboration:
    ///
    /// ```ignore (MIR)
    /// BB0 {
    ///   DropAndReplace(P <- V, goto BB1, unwind BB2)
    /// }
    /// ```
    ///
    /// becomes
    ///
    /// ```ignore (MIR)
    /// BB0 {
    ///   Drop(P, goto BB1, unwind BB2)
    /// }
    /// BB1 {
    ///   // P is now uninitialized
    ///   P <- V
    /// }
    /// BB2 {
    ///   // P is now uninitialized -- its dtor panicked
    ///   P <- V
    /// }
    /// ```
    ///
    /// Disallowed after drop elaboration.
    DropAndReplace {
        place: StoredPlace,
        value: Operand,
        target: BasicBlockId,
        unwind: Option<BasicBlockId>,
    },

    /// Roughly speaking, evaluates the `func` operand and the arguments, and starts execution of
    /// the referred to function. The operand types must match the argument types of the function.
    /// The return place type must match the return type. The type of the `func` operand must be
    /// callable, meaning either a function pointer, a function type, or a closure type.
    ///
    /// **Needs clarification**: The exact semantics of this. Current backends rely on `move`
    /// operands not aliasing the return place. It is unclear how this is justified in MIR, see
    /// [#71117].
    ///
    /// [#71117]: https://github.com/rust-lang/rust/issues/71117
    Call {
        /// The function that’s being called.
        func: Operand,
        /// Arguments the function is called with.
        /// These are owned by the callee, which is free to modify them.
        /// This allows the memory occupied by "by-value" arguments to be
        /// reused across function calls without duplicating the contents.
        args: Box<[Operand]>,
        /// Where the returned value will be written
        destination: StoredPlace,
        /// Where to go after this call returns. If none, the call necessarily diverges.
        target: Option<BasicBlockId>,
        /// Cleanups to be done if the call unwinds.
        cleanup: Option<BasicBlockId>,
        /// `true` if this is from a call in HIR rather than from an overloaded
        /// operator. True for overloaded function call.
        from_hir_call: bool,
        // This `Span` is the span of the function, without the dot and receiver
        // (e.g. `foo(a, b)` in `x.foo(a, b)`
        //fn_span: Span,
    },

    /// Evaluates the operand, which must have type `bool`. If it is not equal to `expected`,
    /// initiates a panic. Initiating a panic corresponds to a `Call` terminator with some
    /// unspecified constant as the function to call, all the operands stored in the `AssertMessage`
    /// as parameters, and `None` for the destination. Keep in mind that the `cleanup` path is not
    /// necessarily executed even in the case of a panic, for example in `-C panic=abort`. If the
    /// assertion does not fail, execution continues at the specified basic block.
    Assert {
        cond: Operand,
        expected: bool,
        msg: AssertKind,
        target: BasicBlockId,
        cleanup: Option<BasicBlockId>,
    },

    /// Marks a suspend point.
    ///
    /// Like `Return` terminators in coroutine bodies, this computes `value` and then a
    /// `CoroutineState::Yielded(value)` as if by `Aggregate` rvalue. That value is then assigned to
    /// the return place of the function calling this one, and execution continues in the calling
    /// function. When next invoked with the same first argument, execution of this function
    /// continues at the `resume` basic block, with the second argument written to the `resume_arg`
    /// place. If the coroutine is dropped before then, the `drop` basic block is invoked.
    ///
    /// Not permitted in bodies that are not coroutine bodies, or after coroutine lowering.
    ///
    /// **Needs clarification**: What about the evaluation order of the `resume_arg` and `value`?
    Yield {
        /// The value to return.
        value: Operand,
        /// Where to resume to.
        resume: BasicBlockId,
        /// The place to store the resume argument in.
        resume_arg: StoredPlace,
        /// Cleanup to be done if the coroutine is dropped at this suspend point.
        drop: Option<BasicBlockId>,
    },

    /// Indicates the end of dropping a coroutine.
    ///
    /// Semantically just a `return` (from the coroutines drop glue). Only permitted in the same situations
    /// as `yield`.
    ///
    /// **Needs clarification**: Is that even correct? The coroutine drop code is always confusing
    /// to me, because it's not even really in the current body.
    ///
    /// **Needs clarification**: Are there type system constraints on these terminators? Should
    /// there be a "block type" like `cleanup` blocks for them?
    CoroutineDrop,

    /// A block where control flow only ever takes one real path, but borrowck needs to be more
    /// conservative.
    ///
    /// At runtime this is semantically just a goto.
    ///
    /// Disallowed after drop elaboration.
    FalseEdge {
        /// The target normal control flow will take.
        real_target: BasicBlockId,
        /// A block control flow could conceptually jump to, but won't in
        /// practice.
        imaginary_target: BasicBlockId,
    },

    /// A terminator for blocks that only take one path in reality, but where we reserve the right
    /// to unwind in borrowck, even if it won't happen in practice. This can arise in infinite loops
    /// with no function calls for example.
    ///
    /// At runtime this is semantically just a goto.
    ///
    /// Disallowed after drop elaboration.
    FalseUnwind {
        /// The target normal control flow will take.
        real_target: BasicBlockId,
        /// The imaginary cleanup block link. This particular path will never be taken
        /// in practice, but in order to avoid fragility we want to always
        /// consider it in borrowck. We don't want to accept programs which
        /// pass borrowck only when `panic=abort` or some assertions are disabled
        /// due to release vs. debug mode builds. This needs to be an `Option` because
        /// of the `remove_noop_landing_pads` and `abort_unwinding_calls` passes.
        unwind: Option<BasicBlockId>,
    },
}

// Order of variants in this enum matter: they are used to compare borrow kinds.
#[derive(Debug, PartialEq, Eq, Clone, Copy, PartialOrd, Ord)]
pub enum BorrowKind {
    /// Data must be immutable and is aliasable.
    Shared,

    /// The immediately borrowed place must be immutable, but projections from
    /// it don't need to be. For example, a shallow borrow of `a.b` doesn't
    /// conflict with a mutable borrow of `a.b.c`.
    ///
    /// This is used when lowering matches: when matching on a place we want to
    /// ensure that place have the same value from the start of the match until
    /// an arm is selected. This prevents this code from compiling:
    /// ```compile_fail,E0510
    /// let mut x = &Some(0);
    /// match *x {
    ///     None => (),
    ///     Some(_) if { x = &None; false } => (),
    ///     Some(_) => (),
    /// }
    /// ```
    /// This can't be a shared borrow because mutably borrowing (*x as Some).0
    /// should not prevent `if let None = x { ... }`, for example, because the
    /// mutating `(*x as Some).0` can't affect the discriminant of `x`.
    /// We can also report errors with this kind of borrow differently.
    Shallow,

    /// Data is mutable and not aliasable.
    Mut { kind: MutBorrowKind },
}

// Order of variants in this enum matter: they are used to compare borrow kinds.
#[derive(Debug, PartialEq, Eq, Clone, Copy, PartialOrd, Ord)]
pub enum MutBorrowKind {
    /// Data must be immutable but not aliasable. This kind of borrow cannot currently
    /// be expressed by the user and is used only in implicit closure bindings.
    ClosureCapture,
    Default,
    /// This borrow arose from method-call auto-ref
    /// (i.e., adjustment::Adjust::Borrow).
    TwoPhasedBorrow,
}

impl BorrowKind {
    pub fn from_hir_mutability(m: hir_def::type_ref::Mutability) -> Self {
        match m {
            hir_def::type_ref::Mutability::Shared => BorrowKind::Shared,
            hir_def::type_ref::Mutability::Mut => BorrowKind::Mut { kind: MutBorrowKind::Default },
        }
    }

    pub fn from_rustc_mutability(m: rustc_ast_ir::Mutability) -> Self {
        match m {
            rustc_ast_ir::Mutability::Not => BorrowKind::Shared,
            rustc_ast_ir::Mutability::Mut => BorrowKind::Mut { kind: MutBorrowKind::Default },
        }
    }

    pub fn from_hir(bk: crate::infer::closure::analysis::BorrowKind) -> Self {
        match bk {
            crate::closure_analysis::BorrowKind::Immutable => Self::Shared,
            crate::closure_analysis::BorrowKind::UniqueImmutable => {
                Self::Mut { kind: MutBorrowKind::ClosureCapture }
            }
            crate::closure_analysis::BorrowKind::Mutable => {
                Self::Mut { kind: MutBorrowKind::Default }
            }
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UnOp {
    /// The `!` operator for logical inversion
    Not,
    /// The `-` operator for negation
    Neg,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum BinOp {
    /// The `+` operator (addition)
    Add,
    /// The `-` operator (subtraction)
    Sub,
    /// The `*` operator (multiplication)
    Mul,
    /// The `/` operator (division)
    ///
    /// Division by zero is UB, because the compiler should have inserted checks
    /// prior to this.
    Div,
    /// The `%` operator (modulus)
    ///
    /// Using zero as the modulus (second operand) is UB, because the compiler
    /// should have inserted checks prior to this.
    Rem,
    /// The `^` operator (bitwise xor)
    BitXor,
    /// The `&` operator (bitwise and)
    BitAnd,
    /// The `|` operator (bitwise or)
    BitOr,
    /// The `<<` operator (shift left)
    ///
    /// The offset is truncated to the size of the first operand before shifting.
    Shl,
    /// The `>>` operator (shift right)
    ///
    /// The offset is truncated to the size of the first operand before shifting.
    Shr,
    /// The `==` operator (equality)
    Eq,
    /// The `<` operator (less than)
    Lt,
    /// The `<=` operator (less than or equal to)
    Le,
    /// The `!=` operator (not equal to)
    Ne,
    /// The `>=` operator (greater than or equal to)
    Ge,
    /// The `>` operator (greater than)
    Gt,
    /// The `ptr.offset` operator
    Offset,
    /// `(a + b, overflowed)`, as rustc's: the check an overflowing `+` gets in runtime MIR.
    AddWithOverflow,
    SubWithOverflow,
    MulWithOverflow,
}

impl BinOp {
    /// Whether the operation panics for some operands: on overflow, when overflow is checked,
    /// or when dividing by zero.
    pub fn may_panic(&self) -> bool {
        matches!(
            self,
            BinOp::Add
                | BinOp::Sub
                | BinOp::Mul
                | BinOp::Div
                | BinOp::Rem
                | BinOp::Shl
                | BinOp::Shr
        )
    }

    pub fn run_compare<T: PartialEq + PartialOrd>(&self, l: T, r: T) -> bool {
        match self {
            BinOp::Ge => l >= r,
            BinOp::Gt => l > r,
            BinOp::Le => l <= r,
            BinOp::Lt => l < r,
            BinOp::Eq => l == r,
            BinOp::Ne => l != r,
            x => panic!("`run_compare` called on operator {x:?}"),
        }
    }
}

impl Display for BinOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Rem => "%",
            BinOp::BitXor => "^",
            BinOp::BitAnd => "&",
            BinOp::BitOr => "|",
            BinOp::Shl => "<<",
            BinOp::Shr => ">>",
            BinOp::Eq => "==",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Ne => "!=",
            BinOp::Ge => ">=",
            BinOp::Gt => ">",
            BinOp::Offset => "`offset`",
            BinOp::AddWithOverflow => "`AddWithOverflow`",
            BinOp::SubWithOverflow => "`SubWithOverflow`",
            BinOp::MulWithOverflow => "`MulWithOverflow`",
        })
    }
}

impl From<hir_def::hir::ArithOp> for BinOp {
    fn from(value: hir_def::hir::ArithOp) -> Self {
        match value {
            hir_def::hir::ArithOp::Add => BinOp::Add,
            hir_def::hir::ArithOp::Mul => BinOp::Mul,
            hir_def::hir::ArithOp::Sub => BinOp::Sub,
            hir_def::hir::ArithOp::Div => BinOp::Div,
            hir_def::hir::ArithOp::Rem => BinOp::Rem,
            hir_def::hir::ArithOp::Shl => BinOp::Shl,
            hir_def::hir::ArithOp::Shr => BinOp::Shr,
            hir_def::hir::ArithOp::BitXor => BinOp::BitXor,
            hir_def::hir::ArithOp::BitOr => BinOp::BitOr,
            hir_def::hir::ArithOp::BitAnd => BinOp::BitAnd,
        }
    }
}

impl From<hir_def::hir::CmpOp> for BinOp {
    fn from(value: hir_def::hir::CmpOp) -> Self {
        match value {
            hir_def::hir::CmpOp::Eq { negated: false } => BinOp::Eq,
            hir_def::hir::CmpOp::Eq { negated: true } => BinOp::Ne,
            hir_def::hir::CmpOp::Ord { ordering: Ordering::Greater, strict: false } => BinOp::Ge,
            hir_def::hir::CmpOp::Ord { ordering: Ordering::Greater, strict: true } => BinOp::Gt,
            hir_def::hir::CmpOp::Ord { ordering: Ordering::Less, strict: false } => BinOp::Le,
            hir_def::hir::CmpOp::Ord { ordering: Ordering::Less, strict: true } => BinOp::Lt,
        }
    }
}

impl From<Operand> for Rvalue {
    fn from(x: Operand) -> Self {
        Self::Use(x)
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum CastKind {
    /// An exposing pointer to address cast. A cast between a pointer and an integer type, or
    /// between a function pointer and an integer type.
    /// See the docs on `expose_addr` for more details.
    PointerExposeAddress,
    /// An address-to-pointer cast that picks up an exposed provenance.
    /// See the docs on `from_exposed_addr` for more details.
    PointerFromExposedAddress,
    /// All sorts of pointer-to-pointer casts. Note that reference-to-raw-ptr casts are
    /// translated into `&raw mut/const *r`, i.e., they are not actually casts.
    PtrToPtr,
    /// Pointer related casts that are done by coercions.
    PointerCoercion(PointerCast),
    /// Cast into a dyn* object.
    DynStar,
    IntToInt,
    FloatToInt,
    FloatToFloat,
    IntToFloat,
    FnPtrToPtr,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Rvalue {
    /// Yields the operand unchanged
    Use(Operand),

    /// Creates an array where each element is the value of the operand.
    ///
    /// Corresponds to source code like `[x; 32]`.
    Repeat(Operand, StoredConst),

    /// Creates a reference of the indicated kind to the place.
    ///
    /// There is not much to document here, because besides the obvious parts the semantics of this
    /// are essentially entirely a part of the aliasing model. There are many UCG issues discussing
    /// exactly what the behavior of this operation should be.
    ///
    /// `Shallow` borrows are disallowed after drop lowering.
    Ref(BorrowKind, StoredPlace),

    /// Creates a pointer/reference to the given thread local.
    ///
    /// The yielded type is a `*mut T` if the static is mutable, otherwise if the static is extern a
    /// `*const T`, and if neither of those apply a `&T`.
    ///
    /// **Note:** This is a runtime operation that actually executes code and is in this sense more
    /// like a function call. Also, eliminating dead stores of this rvalue causes `fn main() {}` to
    /// SIGILL for some reason that I (JakobDegen) never got a chance to look into.
    ///
    /// **Needs clarification**: Are there weird additional semantics here related to the runtime
    /// nature of this operation?
    // ThreadLocalRef(DefId),
    ThreadLocalRef(std::convert::Infallible),

    /// Creates a pointer with the indicated mutability to the place.
    ///
    /// This is generated by pointer casts like `&v as *const _` or raw address of expressions like
    /// `&raw v` or `addr_of!(v)`.
    ///
    /// Like with references, the semantics of this operation are heavily dependent on the aliasing
    /// model.
    // AddressOf(Mutability, Place),
    AddressOf(std::convert::Infallible),

    /// Yields the length of the place, as a `usize`.
    ///
    /// If the type of the place is an array, this is the array length. For slices (`[T]`, not
    /// `&[T]`) this accesses the place's metadata to determine the length. This rvalue is
    /// ill-formed for places of other types.
    Len(StoredPlace),

    /// Performs essentially all of the casts that can be performed via `as`.
    ///
    /// This allows for casts from/to a variety of types.
    ///
    /// **FIXME**: Document exactly which `CastKind`s allow which types of casts. Figure out why
    /// `ArrayToPointer` and `MutToConstPointer` are special.
    Cast(CastKind, Operand, StoredTy),

    // FIXME link to `pointer::offset` when it hits stable.
    /// * `Offset` has the same semantics as `pointer::offset`, except that the second
    ///   parameter may be a `usize` as well.
    /// * The comparison operations accept `bool`s, `char`s, signed or unsigned integers, floats,
    ///   raw pointers, or function pointers and return a `bool`. The types of the operands must be
    ///   matching, up to the usual caveat of the lifetimes in function pointers.
    /// * Left and right shift operations accept signed or unsigned integers not necessarily of the
    ///   same type and return a value of the same type as their LHS. Like in Rust, the RHS is
    ///   truncated as needed.
    /// * The `Bit*` operations accept signed integers, unsigned integers, or bools with matching
    ///   types and return a value of that type.
    /// * The remaining operations accept signed integers, unsigned integers, or floats with
    ///   matching types and return a value of that type.
    ///
    /// From [`MirPhase::Runtime`] on, this is the only binary operation: it never panics, the
    /// checks rustc makes are `Assert`s before it, and `AddWithOverflow` and the like give the
    /// value with whether it overflowed. Built bodies use `CheckedBinaryOp` instead.
    BinaryOp(BinOp, Operand, Operand),

    /// Same as `BinaryOp`, but yields `(T, bool)` with a `bool` indicating an error condition.
    ///
    /// When overflow checking is disabled and we are generating run-time code, the error condition
    /// is false. Otherwise, and always during CTFE, the error condition is determined as described
    /// below.
    ///
    /// For addition, subtraction, and multiplication on integers the error condition is set when
    /// the infinite precision result would be unequal to the actual result.
    ///
    /// For shift operations on integers the error condition is set when the value of right-hand
    /// side is greater than or equal to the number of bits in the type of the left-hand side, or
    /// when the value of right-hand side is negative.
    ///
    /// Other combinations of types and operators are unsupported.
    CheckedBinaryOp(BinOp, Operand, Operand),

    /// Computes a value as described by the operation.
    //NullaryOp(NullOp, Ty),
    NullaryOp(std::convert::Infallible),

    /// Exactly like `BinaryOp`, but less operands.
    ///
    /// Also does two's-complement arithmetic. Negation requires a signed integer or a float;
    /// bitwise not requires a signed integer, unsigned integer, or bool. Both operation kinds
    /// return a value with the same type as their operand.
    UnaryOp(UnOp, Operand),

    /// Computes the discriminant of the place, returning it as an integer of type
    /// `discriminant_ty`. Returns zero for types without discriminant.
    ///
    /// The validity requirements for the underlying value are undecided for this rvalue, see
    /// [#91095]. Note too that the value of the discriminant is not the same thing as the
    /// variant index; use `discriminant_for_variant` to convert.
    ///
    /// [#91095]: https://github.com/rust-lang/rust/issues/91095
    Discriminant(StoredPlace),

    /// Creates an aggregate value, like a tuple or struct.
    ///
    /// This is needed because dataflow analysis needs to distinguish
    /// `dest = Foo { x: ..., y: ... }` from `dest.x = ...; dest.y = ...;` in the case that `Foo`
    /// has a destructor.
    ///
    /// Disallowed after deaggregation for all aggregate kinds except `Array` and `Coroutine`. After
    /// coroutine lowering, `Coroutine` aggregate kinds are disallowed too.
    Aggregate(AggregateKind, Box<[Operand]>),

    /// A CopyForDeref is equivalent to a read from a place at the
    /// codegen level, but is treated specially by drop elaboration. When such a read happens, it
    /// is guaranteed (via nature of the mir_opt `Derefer` in rustc_mir_transform/src/deref_separator)
    /// that the only use of the returned value is a deref operation, immediately
    /// followed by one or more projections. Drop elaboration treats this rvalue as if the
    /// read never happened and just projects further. This allows simplifying various MIR
    /// optimizations and codegen backends that previously had to handle deref operations anywhere
    /// in a place.
    CopyForDeref(StoredPlace),
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum StatementKind {
    Assign(StoredPlace, Rvalue),
    FakeRead(StoredPlace),
    //SetDiscriminant {
    //    place: Box<Place>,
    //    variant_index: VariantIdx,
    //},
    Deinit(StoredPlace),
    StorageLive(LocalId),
    StorageDead(LocalId),
    //Retag(RetagKind, Box<Place>),
    //AscribeUserType(Place, UserTypeProjection, Variance),
    //Intrinsic(Box<NonDivergingIntrinsic>),
    InlineAsm(Box<InlineAsm>),
    Nop,
}

/// An `asm!` block. One that does not return is followed by `Unreachable`.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct InlineAsm {
    /// The pieces name `operands` by index.
    pub template: Box<[hir_def::hir::AsmPiece]>,
    pub operands: Box<[AsmOperand]>,
    pub options: hir_def::hir::AsmOptions,
    /// Where the block says how it ended, when it has labels: 0 for running off its end, and
    /// otherwise the label it jumped to, counted from 1. A `SwitchInt` follows.
    pub taken: Option<StoredPlace>,
    /// The block is the whole of a `#[naked]` function: its code and nothing else.
    pub naked: bool,
    /// The block may overwrite every register a call may (`clobber_abi`).
    pub clobber_abi: bool,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum AsmOperand {
    /// A register that holds `input` when the block starts, and whose value when the block
    /// ends goes to `output`. With neither it is one the block overwrites.
    Reg {
        reg: hir_def::hir::InlineAsmRegOrRegClass,
        input: Option<Operand>,
        output: Option<StoredPlace>,
        /// Whether the block may change the register, which it may with no `output` too.
        written: bool,
    },
    Const(Operand),
    /// A `label` block, which the template jumps to.
    Label,
    /// A function or a static, which the template names.
    Sym(Operand),
}
impl StatementKind {
    pub fn with_span(self, span: MirSpan) -> Statement {
        Statement { kind: self, span }
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Statement {
    pub kind: StatementKind,
    pub span: MirSpan,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BasicBlock {
    /// List of statements in this block.
    pub statements: Vec<Statement>,

    /// Terminator for this block.
    ///
    /// N.B., this should generally ONLY be `None` during construction.
    /// Therefore, you should generally access it via the
    /// `terminator()` or `terminator_mut()` methods. The only
    /// exception is that certain passes, such as `simplify_cfg`, swap
    /// out the terminator temporarily with `None` while they continue
    /// to recurse over the set of basic blocks.
    pub terminator: Option<Terminator>,

    /// If true, this block lies on an unwind path. This is used
    /// during codegen where distinct kinds of basic blocks may be
    /// generated (particularly for MSVC cleanup). Unwind blocks must
    /// only branch to other unwind blocks.
    pub is_cleanup: bool,
}

impl StatementKind {
    /// Whether the statement may panic in a built body: index out of bounds, overflow, division
    /// by zero. From [`MirPhase::Runtime`] on each of these is an `Assert` before the statement,
    /// and no statement panics.
    pub fn may_panic(&self) -> bool {
        let indexes = |place: &StoredPlace| {
            place.projection.as_slice().iter().any(|it| matches!(it, ProjectionElem::Index(_)))
        };
        match self {
            StatementKind::Assign(place, rvalue) => {
                let mut panics = indexes(place);
                rvalue.for_each_operand(&mut |operand| {
                    if let OperandKind::Copy(place) | OperandKind::Move(place) = &operand.kind {
                        panics |= indexes(place);
                    }
                });
                panics
                    || match rvalue {
                        Rvalue::CheckedBinaryOp(op, ..) => op.may_panic(),
                        Rvalue::UnaryOp(UnOp::Neg, _) => true,
                        Rvalue::Ref(_, place)
                        | Rvalue::Len(place)
                        | Rvalue::Discriminant(place)
                        | Rvalue::CopyForDeref(place) => indexes(place),
                        _ => false,
                    }
            }
            StatementKind::FakeRead(place) => indexes(place),
            _ => false,
        }
    }

    pub fn for_each_operand_mut(&mut self, f: &mut impl FnMut(&mut Operand)) {
        match self {
            StatementKind::Assign(_, rvalue) => rvalue.for_each_operand_mut(f),
            StatementKind::InlineAsm(asm) => {
                for operand in &mut asm.operands {
                    match operand {
                        AsmOperand::Reg { input: Some(input), .. } => f(input),
                        AsmOperand::Const(value) | AsmOperand::Sym(value) => f(value),
                        AsmOperand::Reg { input: None, .. } | AsmOperand::Label => (),
                    }
                }
            }
            StatementKind::FakeRead(_)
            | StatementKind::Deinit(_)
            | StatementKind::StorageLive(_)
            | StatementKind::StorageDead(_)
            | StatementKind::Nop => (),
        }
    }

    pub fn for_each_operand(&self, f: &mut impl FnMut(&Operand)) {
        match self {
            StatementKind::Assign(_, rvalue) => rvalue.for_each_operand(f),
            StatementKind::InlineAsm(asm) => {
                for operand in &asm.operands {
                    match operand {
                        AsmOperand::Reg { input: Some(input), .. } => f(input),
                        AsmOperand::Const(value) | AsmOperand::Sym(value) => f(value),
                        AsmOperand::Reg { input: None, .. } | AsmOperand::Label => (),
                    }
                }
            }
            StatementKind::FakeRead(_)
            | StatementKind::Deinit(_)
            | StatementKind::StorageLive(_)
            | StatementKind::StorageDead(_)
            | StatementKind::Nop => (),
        }
    }
}

impl Rvalue {
    pub(crate) fn for_each_operand_mut(&mut self, f: &mut impl FnMut(&mut Operand)) {
        match self {
            Rvalue::Use(it) | Rvalue::Repeat(it, _) | Rvalue::Cast(_, it, _) => f(it),
            Rvalue::UnaryOp(_, it) => f(it),
            Rvalue::CheckedBinaryOp(_, lhs, rhs) | Rvalue::BinaryOp(_, lhs, rhs) => {
                f(lhs);
                f(rhs);
            }
            Rvalue::Aggregate(_, operands) => operands.iter_mut().for_each(f),
            Rvalue::Ref(..)
            | Rvalue::Len(_)
            | Rvalue::Discriminant(_)
            | Rvalue::CopyForDeref(_) => (),
            Rvalue::ThreadLocalRef(n) | Rvalue::AddressOf(n) | Rvalue::NullaryOp(n) => match *n {},
        }
    }

    pub fn for_each_operand(&self, f: &mut impl FnMut(&Operand)) {
        match self {
            Rvalue::Use(it) | Rvalue::Repeat(it, _) | Rvalue::Cast(_, it, _) => f(it),
            Rvalue::UnaryOp(_, it) => f(it),
            Rvalue::CheckedBinaryOp(_, lhs, rhs) | Rvalue::BinaryOp(_, lhs, rhs) => {
                f(lhs);
                f(rhs);
            }
            Rvalue::Aggregate(_, operands) => operands.iter().for_each(f),
            Rvalue::Ref(..)
            | Rvalue::Len(_)
            | Rvalue::Discriminant(_)
            | Rvalue::CopyForDeref(_) => (),
            Rvalue::ThreadLocalRef(n) | Rvalue::AddressOf(n) | Rvalue::NullaryOp(n) => match *n {},
        }
    }
}

impl TerminatorKind {
    pub fn for_each_operand_mut(&mut self, f: &mut impl FnMut(&mut Operand)) {
        match self {
            TerminatorKind::SwitchInt { discr: it, .. }
            | TerminatorKind::Yield { value: it, .. }
            | TerminatorKind::DropAndReplace { value: it, .. } => f(it),
            TerminatorKind::Assert { cond, msg, .. } => {
                f(cond);
                msg.for_each_operand_mut(f);
            }
            TerminatorKind::Call { func, args, .. } => {
                f(func);
                args.iter_mut().for_each(f);
            }
            TerminatorKind::Goto { .. }
            | TerminatorKind::UnwindResume
            | TerminatorKind::Abort
            | TerminatorKind::Return
            | TerminatorKind::Unreachable
            | TerminatorKind::Drop { .. }
            | TerminatorKind::CoroutineDrop
            | TerminatorKind::FalseEdge { .. }
            | TerminatorKind::FalseUnwind { .. } => (),
        }
    }

    pub fn for_each_operand(&self, f: &mut impl FnMut(&Operand)) {
        match self {
            TerminatorKind::SwitchInt { discr: it, .. }
            | TerminatorKind::Yield { value: it, .. }
            | TerminatorKind::DropAndReplace { value: it, .. } => f(it),
            TerminatorKind::Assert { cond, msg, .. } => {
                f(cond);
                msg.for_each_operand(f);
            }
            TerminatorKind::Call { func, args, .. } => {
                f(func);
                args.iter().for_each(f);
            }
            TerminatorKind::Goto { .. }
            | TerminatorKind::UnwindResume
            | TerminatorKind::Abort
            | TerminatorKind::Return
            | TerminatorKind::Unreachable
            | TerminatorKind::Drop { .. }
            | TerminatorKind::CoroutineDrop
            | TerminatorKind::FalseEdge { .. }
            | TerminatorKind::FalseUnwind { .. } => (),
        }
    }

    /// The blocks control goes to from here when nothing unwinds, and the coroutine is not
    /// dropped where it yields.
    pub fn successors(&self) -> SmallVec<[BasicBlockId; 2]> {
        match self {
            TerminatorKind::Goto { target }
            | TerminatorKind::Drop { target, .. }
            | TerminatorKind::DropAndReplace { target, .. }
            | TerminatorKind::Assert { target, .. }
            | TerminatorKind::FalseEdge { real_target: target, .. }
            | TerminatorKind::FalseUnwind { real_target: target, .. } => smallvec![*target],
            TerminatorKind::SwitchInt { targets, .. } => targets.all_targets().into(),
            TerminatorKind::Call { target, .. } => target.iter().copied().collect(),
            TerminatorKind::Yield { resume, drop, .. } => {
                std::iter::once(*resume).chain(*drop).collect()
            }
            TerminatorKind::UnwindResume
            | TerminatorKind::Abort
            | TerminatorKind::Return
            | TerminatorKind::Unreachable
            | TerminatorKind::CoroutineDrop => smallvec![],
        }
    }

    /// The cleanup block this terminator unwinds to, if it names one.
    pub fn unwind(&self) -> Option<BasicBlockId> {
        match self {
            TerminatorKind::Drop { unwind, .. }
            | TerminatorKind::DropAndReplace { unwind, .. }
            | TerminatorKind::FalseUnwind { unwind, .. } => *unwind,
            TerminatorKind::Call { cleanup, .. } | TerminatorKind::Assert { cleanup, .. } => {
                *cleanup
            }
            _ => None,
        }
    }

    pub fn unwind_mut(&mut self) -> Option<&mut Option<BasicBlockId>> {
        match self {
            TerminatorKind::Drop { unwind, .. }
            | TerminatorKind::DropAndReplace { unwind, .. }
            | TerminatorKind::FalseUnwind { unwind, .. } => Some(unwind),
            TerminatorKind::Call { cleanup, .. } | TerminatorKind::Assert { cleanup, .. } => {
                Some(cleanup)
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, SalsaValue)]
pub struct MirBody<'db> {
    pub basic_blocks: Arena<BasicBlock>,
    pub locals: Arena<Local>,
    pub start_block: BasicBlockId,
    pub owner: InferBodyId<'db>,
    pub binding_locals: ArenaMap<BindingId, LocalId>,
    pub upvar_locals: FxHashMap<BindingId, Vec<(LocalId, crate::closure_analysis::Place)>>,
    pub param_locals: Vec<LocalId>,
    /// Temporaries that hold a constant and are borrowed for `'static`, as in `&[1, 2]`. The
    /// value has to outlive the call.
    pub promoted_locals: Vec<(LocalId, ExprId)>,
    /// This field stores the closures directly owned by this body. It is used
    /// in traversing every mir body.
    pub closures: Vec<InternedClosureId<'db>>,
    /// Set for the body of an `async` block, which is compiled as its `Future::poll`.
    pub coroutine: Option<CoroutineLocals>,
    /// Blocks that rustc builds no way into: where a match arm binds that its tree of tests
    /// never gets to. They run if control gets there, and what is moved out in them does not
    /// count for the order a value is dropped in.
    pub unreached: Vec<BasicBlockId>,
    /// The locals that scopes hold, in the order the scopes end: the innermost scope first,
    /// and within a scope the local declared last first. Of two locals that hold a value at
    /// once, the earlier one here is dropped first, also by a landing pad.
    pub scope_ends: Vec<LocalId>,
    /// The body is written with `#[rustc_inherit_overflow_checks]`: its arithmetic is checked
    /// when the crate that compiles it checks, whatever the crate it is written in does.
    pub inherits_overflow_checks: bool,
    /// Which passes the body has been through: see [`MirPhase`].
    pub phase: MirPhase,
    /// From [`MirPhase::Runtime`] on, in the body of an `async` block: the locals lowering
    /// copied each capture into, which are the captures themselves, the field of the future
    /// with the index next to each. They hold their value when the body starts and are kept in
    /// the future's prefix, as rustc uses captures in place (`(_1.0)`).
    pub upvars_in_place: Vec<(LocalId, u32)>,
    /// In a built body, the cleanup block each statement that may panic unwinds to, by its
    /// block and index: where the `Assert` rustc has before it unwinds. The pass that makes
    /// those `Assert`s (`transform::asserts`) uses it up.
    pub panic_unwinds: Vec<(BasicBlockId, usize, BasicBlockId)>,
    /// From [`MirPhase::Runtime`] on, the promoted locals rustc promotes: of a type with
    /// nothing to drop and no cell in it. rustc's `PromoteTemps` makes the borrow of such a
    /// temporary a constant and the temporary goes, so it is never kept across a yield; the
    /// code generator gives it read-only or static memory.
    pub promoted_constants: Vec<LocalId>,
}

/// What a body means, which depends on the passes it has been through. The IDE only ever
/// asks for [`MirPhase::Built`]; the interpreter and the code generator run
/// [`MirPhase::Runtime`], which is computed on its own (`runtime_mir_body` and the like).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, salsa::SalsaValue)]
pub enum MirPhase {
    /// As lowering builds it, as rustc's MIR building does. A `Drop` drops what the place holds
    /// if it holds anything, which the code that runs it has to keep track of. Calls and drops
    /// unwind into the scopes' unwind drop tree: cleanup blocks that drop what is in scope,
    /// innermost first, and in a coroutine end the storage of each, then `UnwindResume`. A
    /// statement may panic (`CheckedBinaryOp`, an index), unwinding where
    /// [`MirBody::panic_unwinds`] says.
    #[default]
    Built,
    /// After drop elaboration, as rustc's `MirPhase::Runtime`. Every `Drop` drops, and
    /// whether it runs is decided by the blocks around it, which read drop flags that are
    /// ordinary `bool` locals. What panics is an `Assert`, with an unwind edge of its own, and
    /// no statement panics. A drop in a cleanup block that unwinds aborts.
    Runtime,
}

/// The locals an `async` body has on top of the ones it declares. Its parameters are a
/// reference to the future and the `&mut Context` of the current `poll`, and the return slot
/// has the type of the block's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, salsa::SalsaValue)]
pub struct CoroutineLocals {
    /// The `Poll` that `Return` and `Yield` hand to the caller of `poll`.
    pub poll: LocalId,
    pub context: LocalId,
}

/// A walk over every place a body mentions, as `walk_places` (each place can be changed) or
/// `visit_places` (each is only read).
macro_rules! places_walker {
    ($name:ident, $statement:ident, $terminator:ident, $iter:ident, $each:ident $(, $m:tt)?) => {
        pub fn $name(&$($m)? self, mut f: impl FnMut(&$($m)? StoredPlace)) {
            for (_, block) in self.basic_blocks.$iter() {
                for statement in &$($m)? block.statements {
                    Self::$statement(&$($m)? statement.kind, &mut f);
                }
                if let Some(terminator) = &$($m)? block.terminator {
                    Self::$terminator(&$($m)? terminator.kind, &mut f);
                }
            }
        }

        /// The places one statement mentions, as the walk over the body has them.
        pub fn $statement(kind: &$($m)? StatementKind, mut f: &mut impl FnMut(&$($m)? StoredPlace)) {
            fn for_operand(op: &$($m)? Operand, f: &mut impl FnMut(&$($m)? StoredPlace)) {
                match &$($m)? op.kind {
                    OperandKind::Copy(p) | OperandKind::Move(p) => {
                        f(p);
                    }
                    OperandKind::Constant { .. }
                    | OperandKind::Static(_)
                    | OperandKind::Allocation { .. } => (),
                }
            }
                    match kind {
                        StatementKind::Assign(p, r) => {
                            f(p);
                            match r {
                                Rvalue::UnaryOp(_, o)
                                | Rvalue::Cast(_, o, _)
                                | Rvalue::Repeat(o, _)
                                | Rvalue::Use(o) => for_operand(o, &mut f),
                                Rvalue::CopyForDeref(p)
                                | Rvalue::Discriminant(p)
                                | Rvalue::Len(p)
                                | Rvalue::Ref(_, p) => f(p),
                                Rvalue::CheckedBinaryOp(_, o1, o2) | Rvalue::BinaryOp(_, o1, o2) => {
                                    for_operand(o1, &mut f);
                                    for_operand(o2, &mut f);
                                }
                                Rvalue::Aggregate(_, ops) => {
                                    for op in ops.$iter() {
                                        for_operand(op, &mut f);
                                    }
                                }
                                Rvalue::ThreadLocalRef(n)
                                | Rvalue::AddressOf(n)
                                | Rvalue::NullaryOp(n) => match *n {},
                            }
                        }
                        StatementKind::FakeRead(p) | StatementKind::Deinit(p) => f(p),
                        StatementKind::InlineAsm(asm) => {
                            for operand in &$($m)? asm.operands {
                                match operand {
                                    AsmOperand::Reg { input, output, .. } => {
                                        if let Some(input) = input {
                                            for_operand(input, &mut f);
                                        }
                                        if let Some(output) = output {
                                            f(output);
                                        }
                                    }
                                    AsmOperand::Const(value) | AsmOperand::Sym(value) => {
                                        for_operand(value, &mut f)
                                    }
                                    AsmOperand::Label => (),
                                }
                            }
                            if let Some(taken) = &$($m)? asm.taken {
                                f(taken);
                            }
                        }
                        StatementKind::StorageLive(_)
                        | StatementKind::StorageDead(_)
                        | StatementKind::Nop => (),
                    }
        }

        /// The places one terminator mentions, as the walk over the body has them.
        pub fn $terminator(kind: &$($m)? TerminatorKind, mut f: &mut impl FnMut(&$($m)? StoredPlace)) {
            fn for_operand(op: &$($m)? Operand, f: &mut impl FnMut(&$($m)? StoredPlace)) {
                match &$($m)? op.kind {
                    OperandKind::Copy(p) | OperandKind::Move(p) => {
                        f(p);
                    }
                    OperandKind::Constant { .. }
                    | OperandKind::Static(_)
                    | OperandKind::Allocation { .. } => (),
                }
            }
                    match kind {
                        TerminatorKind::SwitchInt { discr, .. } => for_operand(discr, &mut f),
                        TerminatorKind::FalseEdge { .. }
                        | TerminatorKind::FalseUnwind { .. }
                        | TerminatorKind::Goto { .. }
                        | TerminatorKind::UnwindResume
                        | TerminatorKind::CoroutineDrop
                        | TerminatorKind::Abort
                        | TerminatorKind::Return
                        | TerminatorKind::Unreachable => (),
                        TerminatorKind::Drop { place, .. } => {
                            f(place);
                        }
                        TerminatorKind::DropAndReplace { place, value, .. } => {
                            f(place);
                            for_operand(value, &mut f);
                        }
                        TerminatorKind::Call { func, args, destination, .. } => {
                            for_operand(func, &mut f);
                            args.$iter().for_each(|x| for_operand(x, &mut f));
                            f(destination);
                        }
                        TerminatorKind::Assert { cond, msg, .. } => {
                            for_operand(cond, &mut f);
                            msg.$each(&mut |it| for_operand(it, &mut f));
                        }
                        TerminatorKind::Yield { value, resume_arg, .. } => {
                            for_operand(value, &mut f);
                            f(resume_arg);
                        }
                    }
        }
    };
}

impl MirBody<'_> {
    /// The future the body of an `async` block waits for at `point`: going on from there
    /// starts by borrowing it, to poll it again.
    pub fn awaited(&self, point: &YieldPoint) -> Option<LocalId> {
        let assigned =
            self.basic_blocks[point.resume].statements.iter().find_map(|it| match &it.kind {
                StatementKind::Assign(_, rvalue) => Some(rvalue),
                _ => None,
            })?;
        match assigned {
            Rvalue::Ref(_, future) => Some(future.local),
            _ => None,
        }
    }

    /// The locals that hold a value when the body starts: its parameters, and in an `async`
    /// body the captures it uses in place.
    pub fn entry_locals(&self) -> impl Iterator<Item = LocalId> + '_ {
        self.param_locals.iter().copied().chain(self.upvars_in_place.iter().map(|it| it.0))
    }

    pub fn local_to_binding_map(&self) -> ArenaMap<LocalId, BindingId> {
        self.binding_locals.iter().map(|(it, y)| (*y, it)).collect()
    }

    places_walker!(
        walk_places,
        walk_statement_places,
        walk_terminator_places,
        iter_mut,
        for_each_operand_mut,
        mut
    );
    places_walker!(
        visit_places,
        visit_statement_places,
        visit_terminator_places,
        iter,
        for_each_operand
    );
}

impl<'db> MirBody<'db> {
    /// Says `Move` for every operand that reads a place whose type is not `Copy`, as rustc's
    /// MIR building does: lowering writes `Copy` for each, and whether a use of a place moves
    /// out of it depends on its type alone.
    /// `copy` is what is known already of which types are `Copy`.
    pub fn mark_moves(
        &mut self,
        infcx: &InferCtxt<'db>,
        env: ParamEnv<'db>,
        copy: &mut FxHashMap<Ty<'db>, bool>,
    ) {
        let mut blocks = std::mem::take(&mut self.basic_blocks);
        let mut mark = |operand: &mut Operand| {
            let OperandKind::Copy(place) = &operand.kind else { return };
            let ty = place.as_ref().ty(self, infcx, env).ty;
            let is_copy =
                *copy.entry(ty).or_insert_with(|| infcx.type_is_copy_modulo_regions(env, ty));
            if !is_copy {
                operand.kind = OperandKind::Move(place.clone());
            }
        };
        for (_, block) in blocks.iter_mut() {
            for statement in &mut block.statements {
                statement.kind.for_each_operand_mut(&mut mark);
            }
            if let Some(terminator) = &mut block.terminator {
                terminator.kind.for_each_operand_mut(&mut mark);
            }
        }
        self.basic_blocks = blocks;
    }
}

impl MirBody<'_> {
    pub fn shrink_to_fit(&mut self) {
        let MirBody {
            basic_blocks,
            locals,
            start_block: _,
            owner: _,
            binding_locals,
            upvar_locals,
            param_locals,
            promoted_locals,
            closures,
            coroutine: _,
            unreached,
            scope_ends,
            inherits_overflow_checks: _,
            phase: _,
            upvars_in_place,
            panic_unwinds,
            promoted_constants,
        } = self;
        promoted_constants.shrink_to_fit();
        upvars_in_place.shrink_to_fit();
        panic_unwinds.shrink_to_fit();
        unreached.shrink_to_fit();
        scope_ends.shrink_to_fit();
        promoted_locals.shrink_to_fit();
        basic_blocks.shrink_to_fit();
        locals.shrink_to_fit();
        binding_locals.shrink_to_fit();
        upvar_locals.shrink_to_fit();
        param_locals.shrink_to_fit();
        closures.shrink_to_fit();
        for (_, b) in basic_blocks.iter_mut() {
            let BasicBlock { statements, terminator: _, is_cleanup: _ } = b;
            statements.shrink_to_fit();
        }
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy, SalsaValue)]
pub enum MirSpan {
    ExprId(ExprId),
    /// Where the expression ends: what leaving a scope does is taken to be written there.
    ExprEnd(ExprId),
    PatId(PatId),
    BindingId(BindingId),
    SelfParam,
    Unknown,
}
impl_from!(ExprId, PatId for MirSpan);

impl MirSpan {
    /// The end of what this is the whole of.
    pub fn end(self) -> MirSpan {
        match self {
            MirSpan::ExprId(expr) => MirSpan::ExprEnd(expr),
            other => other,
        }
    }
}

impl From<&ExprId> for MirSpan {
    fn from(value: &ExprId) -> Self {
        (*value).into()
    }
}

impl<'tcx> Place<'tcx> {
    /// If this place represents a local variable like `_X` with no
    /// projections, return `Some(_X)`.
    #[inline]
    pub fn as_local(&self) -> Option<LocalId> {
        match *self {
            Place { local, projection } if projection.as_slice().is_empty() => Some(local),
            _ => None,
        }
    }
}

/// To determine the type of a place, we need to keep track of the variant that has been downcast to, in order to find the correct fields.
/// This type does that.
#[derive(Copy, Clone, Debug, TypeFoldable, TypeVisitable, Hash, PartialEq, Eq)]
pub struct PlaceTy<'db> {
    pub ty: Ty<'db>,
    /// Downcast to a particular variant of an enum or a coroutine, if included.
    #[type_foldable(identity)]
    #[type_visitable(ignore)]
    pub variant_id: Option<VariantId>,
}

impl<'db> PlaceTy<'db> {
    #[inline]
    pub fn from_ty(ty: Ty<'db>) -> PlaceTy<'db> {
        PlaceTy { ty, variant_id: None }
    }

    pub fn multi_projection_ty(
        self,
        infcx: &InferCtxt<'db>,
        env: ParamEnv<'db>,
        elems: &[PlaceElem],
    ) -> PlaceTy<'db> {
        elems.iter().fold(self, |place_ty, elem| place_ty.projection_ty(infcx, elem, env))
    }

    fn field_ty(
        infcx: &InferCtxt<'db>,
        self_ty: Ty<'db>,
        variant: Option<VariantId>,
        f: FieldIndex,
    ) -> Ty<'db> {
        if let Some(variant_id) = variant {
            match self_ty.kind() {
                TyKind::Adt(adt_def, args) if adt_def.is_enum() => {
                    infcx.interner.db().field_types(variant_id)[f.to_local_field_id()]
                        .ty()
                        .instantiate(infcx.interner, args)
                        .skip_norm_wip()
                }
                // FIXME TyKind::Coroutine...
                _ => panic!("can't downcast non-adt non-coroutine type: {self_ty:?}"),
            }
        } else {
            match self_ty.kind() {
                TyKind::Adt(adt_def, args) if !adt_def.is_enum() => {
                    let variant_id = VariantId::from_non_enum(adt_def.def_id()).unwrap();
                    infcx.interner.db().field_types(variant_id)[f.to_local_field_id()]
                        .ty()
                        .instantiate(infcx.interner, args)
                        .skip_norm_wip()
                }
                TyKind::Closure(_, args) => {
                    args.as_closure().tupled_upvars_ty().tuple_fields()[f.0 as usize]
                }
                TyKind::Coroutine(_, args) => {
                    args.as_coroutine().tupled_upvars_ty().tuple_fields()[f.0 as usize]
                }
                TyKind::CoroutineClosure(_, args) => {
                    args.as_coroutine_closure().tupled_upvars_ty().tuple_fields()[f.0 as usize]
                }
                TyKind::Tuple(tys) => tys
                    .get(f.0 as usize)
                    .cloned()
                    .unwrap_or_else(|| panic!("field {f:?} out of range: {self_ty:?}")),
                _ => panic!("can't project out of {self_ty:?}"),
            }
        }
    }

    /// Convenience wrapper around `projection_ty_core` for `PlaceElem`.
    pub fn projection_ty<V: ::std::fmt::Debug + PartialEq>(
        self,
        infcx: &InferCtxt<'db>,
        elem: &ProjectionElem<V>,
        env: ParamEnv<'db>,
    ) -> PlaceTy<'db> {
        self.projection_ty_core(
            infcx.interner,
            elem,
            |ty| {
                if matches!(ty.kind(), TyKind::Alias(..)) {
                    let mut ocx = ObligationCtxt::new(infcx);
                    match ocx.structurally_normalize_ty(&ObligationCause::dummy(), env, ty) {
                        Ok(it) => it,
                        Err(_) => Ty::new_error(infcx.interner, ErrorGuaranteed),
                    }
                } else {
                    ty
                }
            },
            |self_ty, variant, field_id| Self::field_ty(infcx, self_ty, variant, field_id),
        )
    }

    /// `place_ty.projection_ty_core(tcx, elem, |...| { ... })`
    /// projects `place_ty` onto `elem`, returning the appropriate
    /// `Ty` or downcast variant corresponding to that projection.
    /// The `handle_field` callback must map a `FieldIndex` to its `Ty`
    pub fn projection_ty_core<V: PartialEq + ::std::fmt::Debug>(
        self,
        tcx: DbInterner<'db>,
        elem: &ProjectionElem<V>,
        mut structurally_normalize: impl FnMut(Ty<'db>) -> Ty<'db>,
        mut handle_field: impl FnMut(Ty<'db>, Option<VariantId>, FieldIndex /*, T*/) -> Ty<'db>,
    ) -> PlaceTy<'db> {
        // we only bail on mir building when there are type mismatches
        // but error types may pop up resulting in us still attempting to build the mir
        // so just propagate the error type
        if self.ty.is_ty_error() {
            return PlaceTy::from_ty(Ty::new_error(tcx, ErrorGuaranteed));
        }
        if self.variant_id.is_some() && !matches!(elem, ProjectionElem::Field(..)) {
            panic!("cannot use non field projection on downcasted place")
        }
        match *elem {
            ProjectionElem::Deref => {
                // Lowering a deref pattern on a smart pointer gets here, for one.
                let ty = structurally_normalize(self.ty)
                    .builtin_deref(true)
                    .unwrap_or_else(|| Ty::new_error(tcx, ErrorGuaranteed));
                PlaceTy::from_ty(ty)
            }
            ProjectionElem::Index(_) | ProjectionElem::ConstantIndex { .. } => PlaceTy::from_ty(
                structurally_normalize(self.ty)
                    .builtin_index()
                    .unwrap_or_else(|| Ty::new_error(tcx, ErrorGuaranteed)),
            ),
            ProjectionElem::Subslice { from, to /*, from_end*/ } => {
                PlaceTy::from_ty(match structurally_normalize(self.ty).kind() {
                    TyKind::Slice(..) => self.ty,
                    // `to` counts from the end.
                    TyKind::Array(inner, size) => Ty::new_array_opt(
                        tcx,
                        inner,
                        crate::consteval::try_const_usize(tcx.db, size)
                            .and_then(|size| size.checked_sub(from as u128 + to as u128)),
                    ),
                    _ => panic!("cannot subslice non-array type: `{:?}`", self),
                })
            }
            ProjectionElem::Downcast(index) => PlaceTy { ty: self.ty, variant_id: Some(index) },
            ProjectionElem::Field(f) => {
                PlaceTy::from_ty(handle_field(structurally_normalize(self.ty), self.variant_id, f))
            }
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, GenericTypeVisitable)]
pub struct VTableMap<'db> {
    ty_to_id: FxHashMap<Ty<'db>, usize>,
    id_to_ty: Vec<Ty<'db>>,
}

impl<'db> VTableMap<'db> {
    const OFFSET: usize = 1000; // We should add some offset to ids to make 0 (null) an invalid id.

    pub fn id(&mut self, ty: Ty<'db>) -> usize {
        if let Some(it) = self.ty_to_id.get(&ty) {
            return *it;
        }
        let id = self.id_to_ty.len() + VTableMap::OFFSET;
        self.id_to_ty.push(ty);
        self.ty_to_id.insert(ty, id);
        id
    }

    pub fn ty(&self, id: usize) -> Result<Ty<'db>, MirEvalError<'db>> {
        id.checked_sub(VTableMap::OFFSET)
            .and_then(|id| self.id_to_ty.get(id).copied())
            .ok_or(MirEvalError::InvalidVTableId(id))
    }

    pub fn shrink_to_fit(&mut self) {
        self.id_to_ty.shrink_to_fit();
        self.ty_to_id.shrink_to_fit();
    }

    pub fn is_empty(&self) -> bool {
        self.id_to_ty.is_empty() && self.ty_to_id.is_empty()
    }
}

#[derive(Clone, PartialEq, Eq, SalsaValue)]
pub enum MirEvalError<'db> {
    ConstEvalError(String, Box<ConstEvalError<'db>>),
    LayoutError(LayoutError, StoredTy),
    TargetDataLayoutNotAvailable(TargetLoadError),
    /// Means that code had undefined behavior. We don't try to actively detect UB, but if it was detected
    /// then use this type of error.
    UndefinedBehavior(String),
    Panic(String),
    // FIXME: This should be folded into ConstEvalError?
    MirLowerError(FunctionId, MirLowerError<'db>),
    MirLowerErrorForClosure(InternedClosureId<'db>, MirLowerError<'db>),
    TypeIsUnsized(StoredTy, &'static str),
    NotSupported(String),
    InvalidConst,
    InFunction(
        Box<MirEvalError<'db>>,
        Vec<(Either<FunctionId, InternedClosureId<'db>>, MirSpan, InferBodyId<'db>)>,
    ),
    /// The evaluation spent all of its budget ([`base_db::ConstEvalBudget`]), at `at`: the
    /// body it was in and the place in it, when known.
    OverBudget {
        spent: Budget,
        at: Option<(InferBodyId<'db>, MirSpan)>,
    },
    /// The value of the constant points to memory allocated while evaluating it: by the
    /// program's allocator (a `Box`, `Vec` or `String`), which it would free at run time, or
    /// by `const_allocate` without `const_make_global`.
    HeapInValue {
        of_const_allocate: bool,
    },
    StackOverflow,
    /// FIXME: Fold this into InternalError
    InvalidVTableId(usize),
    /// ?
    CoerceUnsizedError(StoredTy),
    /// These should not occur, usually indicates a bug in mir lowering.
    InternalError(Box<str>),
}

impl std::fmt::Debug for MirEvalError<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConstEvalError(arg0, arg1) => {
                f.debug_tuple("ConstEvalError").field(arg0).field(arg1).finish()
            }
            Self::LayoutError(arg0, arg1) => {
                f.debug_tuple("LayoutError").field(arg0).field(arg1).finish()
            }
            Self::UndefinedBehavior(arg0) => {
                f.debug_tuple("UndefinedBehavior").field(arg0).finish()
            }
            Self::Panic(msg) => write!(f, "Panic with message:\n{msg:?}"),
            Self::TargetDataLayoutNotAvailable(arg0) => {
                f.debug_tuple("TargetDataLayoutNotAvailable").field(arg0).finish()
            }
            Self::TypeIsUnsized(ty, it) => write!(f, "{ty:?} is unsized. {it} should be sized."),
            Self::OverBudget { spent, at: _ } => write!(f, "{spent}"),
            Self::HeapInValue { of_const_allocate: true } => f.write_str(
                "encountered `const_allocate` pointer in final value that was not made global",
            ),
            Self::HeapInValue { of_const_allocate: false } => f.write_str(
                "the value points to memory allocated while evaluating it (a `Box`, `Vec`, \
                 `String` or the like), which the program would free",
            ),
            Self::StackOverflow => write!(f, "stack overflow"),
            Self::MirLowerError(arg0, arg1) => {
                f.debug_tuple("MirLowerError").field(arg0).field(arg1).finish()
            }
            Self::MirLowerErrorForClosure(arg0, arg1) => {
                f.debug_tuple("MirLowerError").field(arg0).field(arg1).finish()
            }
            Self::CoerceUnsizedError(arg0) => {
                f.debug_tuple("CoerceUnsizedError").field(arg0).finish()
            }
            Self::InternalError(arg0) => f.debug_tuple("InternalError").field(arg0).finish(),
            Self::InvalidVTableId(arg0) => f.debug_tuple("InvalidVTableId").field(arg0).finish(),
            Self::NotSupported(arg0) => f.debug_tuple("NotSupported").field(arg0).finish(),
            Self::InvalidConst => f.write_str("InvalidConst"),
            Self::InFunction(e, stack) => {
                f.debug_struct("WithStack").field("error", e).field("stack", &stack).finish()
            }
        }
    }
}

/// What an evaluation ran out of: its budget of steps or of bytes, of this size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, SalsaValue)]
pub enum Budget {
    Steps(u64),
    Memory(u64),
}

impl std::fmt::Display for Budget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Budget::Steps(steps) => write!(f, "exceeded {steps} steps"),
            Budget::Memory(bytes) => write!(f, "exceeded {bytes} bytes of memory"),
        }
    }
}

impl Budget {
    /// The setting under `[workspace.metadata.blaze]` that raises this budget.
    pub fn setting(&self) -> String {
        let key = base_db::ConstEvalBudget::KEY;
        match self {
            Budget::Steps(_) => format!("`{key}.steps`"),
            Budget::Memory(_) => format!("`{key}.memory`"),
        }
    }
}

#[derive(PartialEq, Eq)]
pub enum IsSigned {
    Yes,
    No,
}

impl From<bool> for IsSigned {
    fn from(value: bool) -> Self {
        if value { Self::Yes } else { Self::No }
    }
}

pub fn pad16(it: &[u8], is_signed: IsSigned) -> [u8; 16] {
    let is_negative = is_signed == IsSigned::Yes && it.last().unwrap_or(&0) > &127;
    let mut res = [if is_negative { 255 } else { 0 }; 16];
    res[..it.len()].copy_from_slice(it);
    res
}

// FIXME: Make this smaller, its stored in database queries
#[derive(Debug, Clone, PartialEq, Eq, SalsaValue)]
pub enum MirLowerError<'db> {
    ConstEvalError(Box<str>, Box<ConstEvalError<'db>>),
    LayoutError(LayoutError),
    IncompleteExpr,
    IncompletePattern,
    /// Trying to lower a trait function, instead of an implementation
    TraitFunctionDefinition(TraitId, Name),
    UnresolvedName {
        owner: ExpressionStoreOwnerId,
        path: Path,
    },
    RecordLiteralWithoutPath,
    UnresolvedMethod(String),
    UnresolvedField,
    MissingFunctionDefinition(InferBodyId<'db>, ExprId),
    HasErrors,
    /// This should never happen. Type mismatch should catch everything.
    TypeError(&'static str),
    NotSupported(String),
    ContinueWithoutLoop,
    BreakWithoutLoop,
    Loop,
    /// Something that should never happen and is definitely a bug, but we don't want to panic if it happened
    ImplementationError(String),
    LangItemNotFound,
    MutatingRvalue,
    UnresolvedLabel,
    UnresolvedUpvar(StoredPlace),
    InaccessibleLocal,

    // monomorphization errors:
    GenericArgNotProvided(GenericParamId, StoredGenericArgs),
}

impl From<LayoutError> for MirLowerError<'_> {
    fn from(value: LayoutError) -> Self {
        MirLowerError::LayoutError(value)
    }
}

/// Whether a future of this type, which the `async` block `coroutine` gives, is one that owns
/// what its `async` closure captured.
pub fn is_by_move_coroutine<'db>(
    db: &'db dyn HirDatabase,
    coroutine: InternedCoroutineId<'db>,
    args: CoroutineArgs<DbInterner<'db>>,
) -> bool {
    let InternedClosure { owner, expr, .. } = coroutine.loc(db);
    let store = ExpressionStore::of(db, owner.expression_store_owner(db));
    let Expr::Closure {
        closure_kind: ClosureKind::Coroutine { source: CoroutineSource::Closure, .. },
        ..
    } = &store[expr]
    else {
        return false;
    };
    let closure = ExpressionStore::closure_for_coroutine(expr);
    let TyKind::CoroutineClosure(_, closure_args) =
        InferenceResult::of(db, owner).expr_ty(closure).kind()
    else {
        return false;
    };
    let once = rustc_type_ir::ClosureKind::FnOnce;
    args.kind_ty().to_opt_closure_kind() == Some(once)
        && closure_args.as_coroutine_closure().kind() != once
}

/// How much of rustc's optimization pipeline runs: rustc's `-Zmir-opt-level`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, salsa::SalsaValue)]
pub enum MirOptLevel {
    /// None: the body is the runtime body.
    Off,
    /// rustc's level 1, what a build at `opt-level=0` gets.
    Debug,
    /// rustc's level 2, what a build at any other `opt-level` gets.
    Release,
}

impl MirOptLevel {
    pub fn from_number(level: u8) -> Option<MirOptLevel> {
        match level {
            0 => Some(MirOptLevel::Off),
            1 => Some(MirOptLevel::Debug),
            2 => Some(MirOptLevel::Release),
            _ => None,
        }
    }

    pub fn number(self) -> u8 {
        match self {
            MirOptLevel::Off => 0,
            MirOptLevel::Debug => 1,
            MirOptLevel::Release => 2,
        }
    }
}
