//! What each of rustc's intrinsics reads of its generic arguments, as rustc's definition of it
//! says: one exhaustive `match` over [`Intrinsic`], so an intrinsic added to the enum does not
//! build until it is classified here.

use hir_ty::intrinsic::Intrinsic;

/// What a call of an intrinsic reads of its generic arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Class {
    /// The facts of its type arguments (layout, fields, drop, freeze) and the values of its
    /// const arguments: `size_of`, `transmute`, the atomics, SIMD, arithmetic.
    Layout,
    /// The full identity of its arguments: what tells two types of one layout apart
    /// (`type_id`, `type_name`, the reflection family), a panic message naming the type, a
    /// function or closure it calls.
    Identity,
    /// Code made for the one instance (automatic differentiation, offloading): never shared.
    PerInstance,
}

/// The class of `intrinsic`.
pub fn classify(intrinsic: Intrinsic) -> Class {
    use Intrinsic::*;
    match intrinsic {
        TypeId
        | TypeName
        | TypeIdArrayLen
        | TypeIdElementTy
        | TypeIdFieldRepresentingType
        | TypeIdFields
        | TypeIdFunctionPtr
        | TypeIdGenerics
        | TypeIdIsSigned
        | TypeIdNonExhaustive
        | TypeIdPointsMutably
        | TypeIdPointsTo
        | TypeIdSizeOf
        | TypeIdTypeOf
        | TypeIdVariantName
        | TypeIdVariantNonExhaustive
        | TypeIdVariants
        | TypeIdVtable
        | FieldOffset
        | FieldRepresentingTypeActualTypeId
        | FieldRepresentingTypeName
        | FieldRepresentingTypeOffset
        | AssertInhabited
        | AssertMemUninitializedValid
        | AssertZeroValid
        | ConstEvalSelect
        | ContractCheckEnsures
        | ContractCheckRequires
        | RustcPeek => Class::Identity,
        Autodiff | Offload => Class::PerInstance,
        Atomic(..)
        | Simd(_)
        | CopySign(_)
        | Maximum(_)
        | Minimum(_)
        | MaximumNumberNsz(_)
        | MinimumNumberNsz(_)
        | AbortImmediate
        | AddWithOverflow
        | AggregateRawPtr
        | AlignOf
        | AlignOfVal
        | AmdgpuDispatchPtr
        | ArithOffset
        | Assume
        | Bitreverse
        | BlackBox
        | BoxNew
        | Breakpoint
        | Bswap
        | CallerLocation
        | CarryingMulAdd
        | CarrylessMul
        | CatchUnwind
        | Ceil
        | CodeviewAnnotation
        | ColdPath
        | CompareBytes
        | ConstAllocate
        | ConstDeallocate
        | ConstMakeGlobal
        | Copy
        | CopyNonoverlapping
        | Cos
        | Ctlz
        | CtlzNonzero
        | Ctpop
        | Cttz
        | CttzNonzero
        | DiscriminantValue
        | DisjointBitor
        | ExactDiv
        | Exp
        | Exp2
        | Fabs
        | FaddAlgebraic
        | FaddFast
        | FdivAlgebraic
        | FdivFast
        | FloatToIntUnchecked
        | Floor
        | Fma
        | FmulAlgebraic
        | FmulFast
        | Fmuladd
        | Forget
        | FremAlgebraic
        | FremFast
        | FsubAlgebraic
        | FsubFast
        | GpuLaunchSizedWorkgroupMem
        | IntegerMax
        | IntegerMin
        | IsValStaticallyKnown
        | Log
        | Log10
        | Log2
        | MulWithOverflow
        | NeedsDrop
        | NontemporalStore
        | OffloadGetNumDevices
        | Offset
        | OffsetOf
        | OverflowChecks
        | Powf
        | Powi
        | PrefetchReadData
        | PrefetchReadInstruction
        | PrefetchWriteData
        | PrefetchWriteInstruction
        | PtrGuaranteedCmp
        | PtrMask
        | PtrMetadata
        | PtrOffsetFrom
        | PtrOffsetFromUnsigned
        | RawEq
        | ReadViaCopy
        | ReturnAddress
        | RotateLeft
        | RotateRight
        | Round
        | RoundTiesEven
        | SaturatingAdd
        | SaturatingSub
        | SelectUnpredictable
        | Sin
        | SizeOf
        | SizeOfVal
        | SliceGetUnchecked
        | Sqrt
        | SubWithOverflow
        | SveTupleCreate2
        | SveTupleCreate3
        | SveTupleCreate4
        | SveTupleGet
        | SveTupleSet
        | ThreeWayCompare
        | Transmute
        | TransmuteUnchecked
        | Trunc
        | TypeIdEq
        | TypedSwapNonoverlapping
        | UbChecks
        | UnalignedVolatileLoad
        | UnalignedVolatileStore
        | UncheckedAdd
        | UncheckedDiv
        | UncheckedFunnelShl
        | UncheckedFunnelShr
        | UncheckedMul
        | UncheckedRem
        | UncheckedShl
        | UncheckedShr
        | UncheckedSub
        | Unreachable
        | VaArg
        | VaCopy
        | VaEnd
        | VariantCount
        | VolatileCopyMemory
        | VolatileCopyNonoverlappingMemory
        | VolatileLoad
        | VolatileSetMemory
        | VolatileStore
        | VtableAlign
        | VtableSize
        | WrappingAdd
        | WrappingMul
        | WrappingSub
        | WriteBoxViaMove
        | WriteBytes
        | WriteViaMove => Class::Layout,
    }
}
