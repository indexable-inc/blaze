//! rustc's intrinsics as one 2-byte enum, [`Intrinsic`]: every `#[rustc_intrinsic]` function of
//! the pinned nightly's library (`core::intrinsics`, its `simd`, `simd::scalable`, `gpu` and
//! `reflection` modules, and `alloc::intrinsics`), and the names older nightlies gave some of
//! them: the atomics with their orderings in the name (`atomic_load_acquire`) and `box_new`.
//!
//! A callee is recognized once, when MIR lowering lowers the call
//! (`TerminatorKind::Call::intrinsic`), from its pre-interned symbol: a pointer subtraction and
//! one load in a [`KnownNames`] table, with no string compared. The interpreter and the back
//! ends then `match` on the enum. A `#[rustc_intrinsic]` whose name is none of these is an
//! error naming it ([`UnknownIntrinsic`]), as it is for rustc (E0093).
//!
//! The older names ([`Intrinsic::is_legacy`]) are an extension: the pinned nightly's rustc
//! refuses them, so accepting them changes no program it builds, and fixtures on minicore, which
//! has no `AtomicOrdering`, call atomics by them. `atomic_load_unordered` and
//! `atomic_store_unordered` are not kept, as no evaluator has an unordered access.
//!
//! The atomic and SIMD families carry their operation, the float intrinsics named by width
//! carry it. An atomic carries the orderings its name gives ([`AtomicOrders`]); the pinned
//! nightly passes them as const generic arguments instead (`atomic_load::<T, ORD>`), and
//! [`atomic_orderings`] reads both forms to the same [`AtomicOrdering`]s.

use std::fmt;

use base_db::Db;
use hir_def::{FunctionId, signatures::FunctionSignature};
use intern::{KnownNames, Symbol, sym, sym::Declared};
use rustc_type_ir::inherent::{IntoKind, SliceLike};

use crate::{
    consteval::try_const_usize,
    db::HirDatabase,
    next_solver::{Const, ConstKind, GenericArgs, ValTreeKind},
};

/// rustc's intrinsic, its family's operation inside: 2 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Intrinsic {
    /// `atomic_*`.
    Atomic(AtomicOp, AtomicOrders),
    /// `simd_*`.
    Simd(SimdOp),
    /// `copysignf16` to `copysignf128`.
    CopySign(FloatWidth),
    /// `maximumf16` to `maximumf128`.
    Maximum(FloatWidth),
    /// `minimumf16` to `minimumf128`.
    Minimum(FloatWidth),
    /// `maximum_number_nsz_f16` to `maximum_number_nsz_f128`.
    MaximumNumberNsz(FloatWidth),
    /// `minimum_number_nsz_f16` to `minimum_number_nsz_f128`.
    MinimumNumberNsz(FloatWidth),
    AbortImmediate, AddWithOverflow, AggregateRawPtr, AlignOf, AlignOfVal, AmdgpuDispatchPtr,
    ArithOffset, AssertInhabited, AssertMemUninitializedValid, AssertZeroValid, Assume, Autodiff,
    Bitreverse, BlackBox, BoxNew, Breakpoint, Bswap, CallerLocation, CarryingMulAdd, CarrylessMul,
    CatchUnwind, Ceil, CodeviewAnnotation, ColdPath, CompareBytes, ConstAllocate, ConstDeallocate,
    ConstEvalSelect, ConstMakeGlobal, ContractCheckEnsures, ContractCheckRequires, Copy,
    CopyNonoverlapping, Cos, Ctlz, CtlzNonzero, Ctpop, Cttz, CttzNonzero, DiscriminantValue,
    DisjointBitor, ExactDiv, Exp, Exp2, Fabs, FaddAlgebraic, FaddFast, FdivAlgebraic, FdivFast,
    FieldOffset, FieldRepresentingTypeActualTypeId, FieldRepresentingTypeName,
    FieldRepresentingTypeOffset, FloatToIntUnchecked, Floor, Fma, FmulAlgebraic, FmulFast, Fmuladd,
    Forget, FremAlgebraic, FremFast, FsubAlgebraic, FsubFast, GpuLaunchSizedWorkgroupMem,
    IntegerMax, IntegerMin, IsValStaticallyKnown, Log, Log10, Log2, MulWithOverflow, NeedsDrop,
    NontemporalStore, Offload, OffloadGetNumDevices, Offset, OffsetOf, OverflowChecks, Powf, Powi,
    PrefetchReadData, PrefetchReadInstruction, PrefetchWriteData, PrefetchWriteInstruction,
    PtrGuaranteedCmp, PtrMask, PtrMetadata, PtrOffsetFrom, PtrOffsetFromUnsigned, RawEq,
    ReadViaCopy, ReturnAddress, RotateLeft, RotateRight, Round, RoundTiesEven, RustcPeek,
    SaturatingAdd, SaturatingSub, SelectUnpredictable, Sin, SizeOf, SizeOfVal, SliceGetUnchecked,
    Sqrt, SubWithOverflow, SveTupleCreate2, SveTupleCreate3, SveTupleCreate4, SveTupleGet,
    SveTupleSet, ThreeWayCompare, Transmute, TransmuteUnchecked, Trunc, TypeId, TypeIdArrayLen,
    TypeIdElementTy, TypeIdEq, TypeIdFieldRepresentingType, TypeIdFields, TypeIdFunctionPtr,
    TypeIdGenerics, TypeIdIsSigned, TypeIdNonExhaustive, TypeIdPointsMutably, TypeIdPointsTo,
    TypeIdSizeOf, TypeIdTypeOf, TypeIdVariantName, TypeIdVariantNonExhaustive, TypeIdVariants,
    TypeIdVtable, TypeName, TypedSwapNonoverlapping, UbChecks, UnalignedVolatileLoad,
    UnalignedVolatileStore, UncheckedAdd, UncheckedDiv, UncheckedFunnelShl, UncheckedFunnelShr,
    UncheckedMul, UncheckedRem, UncheckedShl, UncheckedShr, UncheckedSub, Unreachable, VaArg,
    VaCopy, VaEnd, VariantCount, VolatileCopyMemory, VolatileCopyNonoverlappingMemory,
    VolatileLoad, VolatileSetMemory, VolatileStore, VtableAlign, VtableSize, WrappingAdd,
    WrappingMul, WrappingSub, WriteBoxViaMove, WriteBytes, WriteViaMove,
}

const _: () = assert!(size_of::<Intrinsic>() == 2);
const _: () = assert!(size_of::<Option<Intrinsic>>() == 2);

/// The operation of an `atomic_*` intrinsic, named as after the prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum AtomicOp {
    And, Cxchg, CxchgWeak, Fence, Load, Max, Min, Nand, Or, SingleThreadFence, Store, Umax, Umin,
    Xadd, Xchg, Xor, Xsub,
}

/// Where an atomic intrinsic's orderings are: in its const generic arguments, as the pinned
/// nightly has them, or in an older nightly's name (`atomic_cxchg_acqrel_relaxed`: success, then
/// failure).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum AtomicOrders {
    Generic,
    Relaxed,
    Acquire,
    Release,
    AcqRel,
    SeqCst,
    RelaxedRelaxed, RelaxedAcquire, RelaxedSeqCst, AcquireRelaxed, AcquireAcquire, AcquireSeqCst,
    ReleaseRelaxed, ReleaseAcquire, ReleaseSeqCst, AcqRelRelaxed, AcqRelAcquire, AcqRelSeqCst,
    SeqCstRelaxed, SeqCstAcquire, SeqCstSeqCst,
}

impl AtomicOrders {
    /// The orderings the name gives: the ordering, and a compare-exchange's failure ordering.
    /// `None` for [`AtomicOrders::Generic`].
    pub const fn named(self) -> Option<(AtomicOrdering, Option<AtomicOrdering>)> {
        use AtomicOrders::*;
        match self {
            Generic => None,
            Relaxed => Some((AtomicOrdering::Relaxed, None)),
            Acquire => Some((AtomicOrdering::Acquire, None)),
            Release => Some((AtomicOrdering::Release, None)),
            AcqRel => Some((AtomicOrdering::AcqRel, None)),
            SeqCst => Some((AtomicOrdering::SeqCst, None)),
            RelaxedRelaxed => Some((AtomicOrdering::Relaxed, Some(AtomicOrdering::Relaxed))),
            RelaxedAcquire => Some((AtomicOrdering::Relaxed, Some(AtomicOrdering::Acquire))),
            RelaxedSeqCst => Some((AtomicOrdering::Relaxed, Some(AtomicOrdering::SeqCst))),
            AcquireRelaxed => Some((AtomicOrdering::Acquire, Some(AtomicOrdering::Relaxed))),
            AcquireAcquire => Some((AtomicOrdering::Acquire, Some(AtomicOrdering::Acquire))),
            AcquireSeqCst => Some((AtomicOrdering::Acquire, Some(AtomicOrdering::SeqCst))),
            ReleaseRelaxed => Some((AtomicOrdering::Release, Some(AtomicOrdering::Relaxed))),
            ReleaseAcquire => Some((AtomicOrdering::Release, Some(AtomicOrdering::Acquire))),
            ReleaseSeqCst => Some((AtomicOrdering::Release, Some(AtomicOrdering::SeqCst))),
            AcqRelRelaxed => Some((AtomicOrdering::AcqRel, Some(AtomicOrdering::Relaxed))),
            AcqRelAcquire => Some((AtomicOrdering::AcqRel, Some(AtomicOrdering::Acquire))),
            AcqRelSeqCst => Some((AtomicOrdering::AcqRel, Some(AtomicOrdering::SeqCst))),
            SeqCstRelaxed => Some((AtomicOrdering::SeqCst, Some(AtomicOrdering::Relaxed))),
            SeqCstAcquire => Some((AtomicOrdering::SeqCst, Some(AtomicOrdering::Acquire))),
            SeqCstSeqCst => Some((AtomicOrdering::SeqCst, Some(AtomicOrdering::SeqCst))),
        }
    }
}

/// core's `intrinsics::AtomicOrdering`, with its discriminants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum AtomicOrdering {
    Relaxed = 0,
    Release = 1,
    Acquire = 2,
    AcqRel = 3,
    SeqCst = 4,
}

impl AtomicOrdering {
    /// The ordering whose discriminant is `discriminant`.
    pub const fn from_discriminant(discriminant: u128) -> Option<AtomicOrdering> {
        Some(match discriminant {
            0 => AtomicOrdering::Relaxed,
            1 => AtomicOrdering::Release,
            2 => AtomicOrdering::Acquire,
            3 => AtomicOrdering::AcqRel,
            4 => AtomicOrdering::SeqCst,
            _ => return None,
        })
    }
}

/// The orderings of a call of `Intrinsic::Atomic(op, orders)` with `generic_args`: the ordering,
/// and a compare-exchange's failure ordering. `None` where a const argument is not a known
/// `AtomicOrdering` (a generic body's parameter).
pub fn atomic_orderings<'db>(
    db: &'db Db,
    op: AtomicOp,
    orders: AtomicOrders,
    generic_args: GenericArgs<'db>,
) -> Option<(AtomicOrdering, Option<AtomicOrdering>)> {
    if let Some(named) = orders.named() {
        return Some(named);
    }
    let mut consts = generic_args.iter().filter_map(|arg| arg.konst());
    let ordering = atomic_ordering_of(db, consts.next()?)?;
    let failure = match op {
        AtomicOp::Cxchg | AtomicOp::CxchgWeak => Some(atomic_ordering_of(db, consts.next()?)?),
        _ => None,
    };
    Some((ordering, failure))
}

/// The `AtomicOrdering` a const generic argument is: a value's variant index (core declares the
/// variants in discriminant order), or what an unevaluated constant evaluates to.
pub fn atomic_ordering_of<'db>(
    db: &'db Db,
    ordering: Const<'db>,
) -> Option<AtomicOrdering> {
    let discriminant = match ordering.kind() {
        ConstKind::Value(value) => {
            let ValTreeKind::Branch(fields) = value.value.inner() else { return None };
            let ConstKind::Value(variant) = fields.as_slice().first()?.kind() else {
                return None;
            };
            variant.try_to_leaf()?.to_uint_unchecked()
        }
        ConstKind::Alias(..) => try_const_usize(db, ordering)?,
        _ => return None,
    };
    AtomicOrdering::from_discriminant(discriminant)
}

/// The operation of a `simd_*` intrinsic, named as after the prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum SimdOp {
    Add, And, ArithOffset, As, Bitmask, Bitreverse, Bswap, CarrylessMul, Cast, CastPtr, Ceil, Ctlz,
    Ctpop, Cttz, Div, Eq, ExposeProvenance, Extract, ExtractDyn, Fabs, Fcos, Fexp, Fexp2, Flog,
    Flog10, Flog2, Floor, Fma, Fsin, Fsqrt, FunnelShl, FunnelShr, Gather, Ge, Gt, Insert,
    InsertDyn, Le, Lt, MaskedLoad, MaskedStore, MaximumNumberNsz, MinimumNumberNsz, Mul, Ne, Neg,
    Or, ReduceAddOrdered, ReduceAddUnordered, ReduceAll, ReduceAnd, ReduceAny, ReduceMax,
    ReduceMin, ReduceMulOrdered, ReduceMulUnordered, ReduceOr, ReduceXor, RelaxedFma, Rem, Round,
    RoundTiesEven, SaturatingAdd, SaturatingSub, Scatter, Select, SelectBitmask, Shl, Shr, Shuffle,
    Splat, Sub, Trunc, WithExposedProvenance, Xor,
}

/// The float type a float intrinsic's name ends in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum FloatWidth {
    F16,
    F32,
    F64,
    F128,
}

impl FloatWidth {
    pub const fn bits(self) -> u32 {
        match self {
            FloatWidth::F16 => 16,
            FloatWidth::F32 => 32,
            FloatWidth::F64 => 64,
            FloatWidth::F128 => 128,
        }
    }
}

/// A `#[rustc_intrinsic]` function whose name is none of rustc's intrinsics.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UnknownIntrinsic(pub Symbol);

impl fmt::Display for UnknownIntrinsic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "`{}` is declared `#[rustc_intrinsic]`, but the pinned nightly's rustc has no \
             intrinsic by that name",
            self.0
        )
    }
}

/// The intrinsic `func` is: `None` for a function that is not `#[rustc_intrinsic]`, an error
/// for one whose name is none of rustc's.
pub fn intrinsic_of(
    db: &Db,
    func: FunctionId,
) -> Result<Option<Intrinsic>, UnknownIntrinsic> {
    if !FunctionSignature::is_intrinsic(db, func) {
        return Ok(None);
    }
    let name = FunctionSignature::of(db, func).name.symbol();
    match Intrinsic::from_symbol(name) {
        Some(intrinsic) => Ok(Some(intrinsic)),
        None => Err(UnknownIntrinsic(name.clone())), // clone: error path, the error names it
    }
}

impl Intrinsic {
    /// How many names there are: rustc's intrinsics, and the older names.
    pub const COUNT: usize = BY_INDEX.as_slice().len();

    /// The intrinsic named `name`, if there is one: a pointer subtraction and one load.
    #[inline]
    pub fn from_symbol(name: &Symbol) -> Option<Intrinsic> {
        KNOWN.get(name).map(|index| BY_INDEX[usize::from(index)])
    }

    /// The intrinsic at `index` in the names in byte order.
    #[inline]
    pub fn from_index(index: u16) -> Option<Intrinsic> {
        BY_INDEX.as_slice().get(usize::from(index)).copied()
    }

    /// The name's rank among the names in byte order: dense, below [`Self::COUNT`], so a
    /// set of intrinsics is a bitset.
    pub const fn index(self) -> u16 {
        self.entry().0
    }

    /// The intrinsic's name.
    pub const fn symbol(self) -> Symbol {
        self.entry().1
    }

    /// Whether the name is an older nightly's, which the pinned nightly's rustc refuses.
    pub const fn is_legacy(self) -> bool {
        match self {
            Intrinsic::BoxNew => true,
            Intrinsic::Atomic(_, orders) => !matches!(orders, AtomicOrders::Generic),
            _ => false,
        }
    }
}

impl fmt::Display for Intrinsic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.symbol().as_str())
    }
}

macro_rules! intrinsics {
    ($($index:literal $name:ident => $variant:ident $(($($data:tt)*))?,)+) => {
        /// The intrinsics in their names' byte order: their [`Intrinsic::index`].
        const MEMBERS: &[Declared] = &[$(Declared::$name,)+];

        const BY_INDEX: [Intrinsic; MEMBERS.len()] = [$(Intrinsic::$variant $(($($data)*))?,)+];

        impl Intrinsic {
            const fn entry(self) -> (u16, Symbol) {
                match self {
                    $(Intrinsic::$variant $(($($data)*))? => ($index, sym::$name),)+
                    Intrinsic::Atomic(..) => {
                        panic!("an `Intrinsic::Atomic` whose operation and orderings no name has")
                    }
                }
            }
        }
    };
}

/// [`MEMBERS`] by declared symbol: 2 bytes a declared symbol, one load a lookup.
static KNOWN: KnownNames = KnownNames::new(MEMBERS);

intrinsics! {
    0 abort_immediate => AbortImmediate,
    1 add_with_overflow => AddWithOverflow,
    2 aggregate_raw_ptr => AggregateRawPtr,
    3 align_of => AlignOf,
    4 align_of_val => AlignOfVal,
    5 amdgpu_dispatch_ptr => AmdgpuDispatchPtr,
    6 arith_offset => ArithOffset,
    7 assert_inhabited => AssertInhabited,
    8 assert_mem_uninitialized_valid => AssertMemUninitializedValid,
    9 assert_zero_valid => AssertZeroValid,
    10 assume => Assume,
    11 atomic_and => Atomic(AtomicOp::And, AtomicOrders::Generic),
    12 atomic_and_acqrel => Atomic(AtomicOp::And, AtomicOrders::AcqRel),
    13 atomic_and_acquire => Atomic(AtomicOp::And, AtomicOrders::Acquire),
    14 atomic_and_relaxed => Atomic(AtomicOp::And, AtomicOrders::Relaxed),
    15 atomic_and_release => Atomic(AtomicOp::And, AtomicOrders::Release),
    16 atomic_and_seqcst => Atomic(AtomicOp::And, AtomicOrders::SeqCst),
    17 atomic_cxchg => Atomic(AtomicOp::Cxchg, AtomicOrders::Generic),
    18 atomic_cxchg_acqrel_acquire => Atomic(AtomicOp::Cxchg, AtomicOrders::AcqRelAcquire),
    19 atomic_cxchg_acqrel_relaxed => Atomic(AtomicOp::Cxchg, AtomicOrders::AcqRelRelaxed),
    20 atomic_cxchg_acqrel_seqcst => Atomic(AtomicOp::Cxchg, AtomicOrders::AcqRelSeqCst),
    21 atomic_cxchg_acquire_acquire => Atomic(AtomicOp::Cxchg, AtomicOrders::AcquireAcquire),
    22 atomic_cxchg_acquire_relaxed => Atomic(AtomicOp::Cxchg, AtomicOrders::AcquireRelaxed),
    23 atomic_cxchg_acquire_seqcst => Atomic(AtomicOp::Cxchg, AtomicOrders::AcquireSeqCst),
    24 atomic_cxchg_relaxed_acquire => Atomic(AtomicOp::Cxchg, AtomicOrders::RelaxedAcquire),
    25 atomic_cxchg_relaxed_relaxed => Atomic(AtomicOp::Cxchg, AtomicOrders::RelaxedRelaxed),
    26 atomic_cxchg_relaxed_seqcst => Atomic(AtomicOp::Cxchg, AtomicOrders::RelaxedSeqCst),
    27 atomic_cxchg_release_acquire => Atomic(AtomicOp::Cxchg, AtomicOrders::ReleaseAcquire),
    28 atomic_cxchg_release_relaxed => Atomic(AtomicOp::Cxchg, AtomicOrders::ReleaseRelaxed),
    29 atomic_cxchg_release_seqcst => Atomic(AtomicOp::Cxchg, AtomicOrders::ReleaseSeqCst),
    30 atomic_cxchg_seqcst_acquire => Atomic(AtomicOp::Cxchg, AtomicOrders::SeqCstAcquire),
    31 atomic_cxchg_seqcst_relaxed => Atomic(AtomicOp::Cxchg, AtomicOrders::SeqCstRelaxed),
    32 atomic_cxchg_seqcst_seqcst => Atomic(AtomicOp::Cxchg, AtomicOrders::SeqCstSeqCst),
    33 atomic_cxchgweak => Atomic(AtomicOp::CxchgWeak, AtomicOrders::Generic),
    34 atomic_cxchgweak_acqrel_acquire => Atomic(AtomicOp::CxchgWeak, AtomicOrders::AcqRelAcquire),
    35 atomic_cxchgweak_acqrel_relaxed => Atomic(AtomicOp::CxchgWeak, AtomicOrders::AcqRelRelaxed),
    36 atomic_cxchgweak_acqrel_seqcst => Atomic(AtomicOp::CxchgWeak, AtomicOrders::AcqRelSeqCst),
    37 atomic_cxchgweak_acquire_acquire => Atomic(AtomicOp::CxchgWeak, AtomicOrders::AcquireAcquire),
    38 atomic_cxchgweak_acquire_relaxed => Atomic(AtomicOp::CxchgWeak, AtomicOrders::AcquireRelaxed),
    39 atomic_cxchgweak_acquire_seqcst => Atomic(AtomicOp::CxchgWeak, AtomicOrders::AcquireSeqCst),
    40 atomic_cxchgweak_relaxed_acquire => Atomic(AtomicOp::CxchgWeak, AtomicOrders::RelaxedAcquire),
    41 atomic_cxchgweak_relaxed_relaxed => Atomic(AtomicOp::CxchgWeak, AtomicOrders::RelaxedRelaxed),
    42 atomic_cxchgweak_relaxed_seqcst => Atomic(AtomicOp::CxchgWeak, AtomicOrders::RelaxedSeqCst),
    43 atomic_cxchgweak_release_acquire => Atomic(AtomicOp::CxchgWeak, AtomicOrders::ReleaseAcquire),
    44 atomic_cxchgweak_release_relaxed => Atomic(AtomicOp::CxchgWeak, AtomicOrders::ReleaseRelaxed),
    45 atomic_cxchgweak_release_seqcst => Atomic(AtomicOp::CxchgWeak, AtomicOrders::ReleaseSeqCst),
    46 atomic_cxchgweak_seqcst_acquire => Atomic(AtomicOp::CxchgWeak, AtomicOrders::SeqCstAcquire),
    47 atomic_cxchgweak_seqcst_relaxed => Atomic(AtomicOp::CxchgWeak, AtomicOrders::SeqCstRelaxed),
    48 atomic_cxchgweak_seqcst_seqcst => Atomic(AtomicOp::CxchgWeak, AtomicOrders::SeqCstSeqCst),
    49 atomic_fence => Atomic(AtomicOp::Fence, AtomicOrders::Generic),
    50 atomic_fence_acqrel => Atomic(AtomicOp::Fence, AtomicOrders::AcqRel),
    51 atomic_fence_acquire => Atomic(AtomicOp::Fence, AtomicOrders::Acquire),
    52 atomic_fence_release => Atomic(AtomicOp::Fence, AtomicOrders::Release),
    53 atomic_fence_seqcst => Atomic(AtomicOp::Fence, AtomicOrders::SeqCst),
    54 atomic_load => Atomic(AtomicOp::Load, AtomicOrders::Generic),
    55 atomic_load_acquire => Atomic(AtomicOp::Load, AtomicOrders::Acquire),
    56 atomic_load_relaxed => Atomic(AtomicOp::Load, AtomicOrders::Relaxed),
    57 atomic_load_seqcst => Atomic(AtomicOp::Load, AtomicOrders::SeqCst),
    58 atomic_max => Atomic(AtomicOp::Max, AtomicOrders::Generic),
    59 atomic_max_acqrel => Atomic(AtomicOp::Max, AtomicOrders::AcqRel),
    60 atomic_max_acquire => Atomic(AtomicOp::Max, AtomicOrders::Acquire),
    61 atomic_max_relaxed => Atomic(AtomicOp::Max, AtomicOrders::Relaxed),
    62 atomic_max_release => Atomic(AtomicOp::Max, AtomicOrders::Release),
    63 atomic_max_seqcst => Atomic(AtomicOp::Max, AtomicOrders::SeqCst),
    64 atomic_min => Atomic(AtomicOp::Min, AtomicOrders::Generic),
    65 atomic_min_acqrel => Atomic(AtomicOp::Min, AtomicOrders::AcqRel),
    66 atomic_min_acquire => Atomic(AtomicOp::Min, AtomicOrders::Acquire),
    67 atomic_min_relaxed => Atomic(AtomicOp::Min, AtomicOrders::Relaxed),
    68 atomic_min_release => Atomic(AtomicOp::Min, AtomicOrders::Release),
    69 atomic_min_seqcst => Atomic(AtomicOp::Min, AtomicOrders::SeqCst),
    70 atomic_nand => Atomic(AtomicOp::Nand, AtomicOrders::Generic),
    71 atomic_nand_acqrel => Atomic(AtomicOp::Nand, AtomicOrders::AcqRel),
    72 atomic_nand_acquire => Atomic(AtomicOp::Nand, AtomicOrders::Acquire),
    73 atomic_nand_relaxed => Atomic(AtomicOp::Nand, AtomicOrders::Relaxed),
    74 atomic_nand_release => Atomic(AtomicOp::Nand, AtomicOrders::Release),
    75 atomic_nand_seqcst => Atomic(AtomicOp::Nand, AtomicOrders::SeqCst),
    76 atomic_or => Atomic(AtomicOp::Or, AtomicOrders::Generic),
    77 atomic_or_acqrel => Atomic(AtomicOp::Or, AtomicOrders::AcqRel),
    78 atomic_or_acquire => Atomic(AtomicOp::Or, AtomicOrders::Acquire),
    79 atomic_or_relaxed => Atomic(AtomicOp::Or, AtomicOrders::Relaxed),
    80 atomic_or_release => Atomic(AtomicOp::Or, AtomicOrders::Release),
    81 atomic_or_seqcst => Atomic(AtomicOp::Or, AtomicOrders::SeqCst),
    82 atomic_singlethreadfence => Atomic(AtomicOp::SingleThreadFence, AtomicOrders::Generic),
    83 atomic_singlethreadfence_acqrel => Atomic(AtomicOp::SingleThreadFence, AtomicOrders::AcqRel),
    84 atomic_singlethreadfence_acquire => Atomic(AtomicOp::SingleThreadFence, AtomicOrders::Acquire),
    85 atomic_singlethreadfence_release => Atomic(AtomicOp::SingleThreadFence, AtomicOrders::Release),
    86 atomic_singlethreadfence_seqcst => Atomic(AtomicOp::SingleThreadFence, AtomicOrders::SeqCst),
    87 atomic_store => Atomic(AtomicOp::Store, AtomicOrders::Generic),
    88 atomic_store_relaxed => Atomic(AtomicOp::Store, AtomicOrders::Relaxed),
    89 atomic_store_release => Atomic(AtomicOp::Store, AtomicOrders::Release),
    90 atomic_store_seqcst => Atomic(AtomicOp::Store, AtomicOrders::SeqCst),
    91 atomic_umax => Atomic(AtomicOp::Umax, AtomicOrders::Generic),
    92 atomic_umax_acqrel => Atomic(AtomicOp::Umax, AtomicOrders::AcqRel),
    93 atomic_umax_acquire => Atomic(AtomicOp::Umax, AtomicOrders::Acquire),
    94 atomic_umax_relaxed => Atomic(AtomicOp::Umax, AtomicOrders::Relaxed),
    95 atomic_umax_release => Atomic(AtomicOp::Umax, AtomicOrders::Release),
    96 atomic_umax_seqcst => Atomic(AtomicOp::Umax, AtomicOrders::SeqCst),
    97 atomic_umin => Atomic(AtomicOp::Umin, AtomicOrders::Generic),
    98 atomic_umin_acqrel => Atomic(AtomicOp::Umin, AtomicOrders::AcqRel),
    99 atomic_umin_acquire => Atomic(AtomicOp::Umin, AtomicOrders::Acquire),
    100 atomic_umin_relaxed => Atomic(AtomicOp::Umin, AtomicOrders::Relaxed),
    101 atomic_umin_release => Atomic(AtomicOp::Umin, AtomicOrders::Release),
    102 atomic_umin_seqcst => Atomic(AtomicOp::Umin, AtomicOrders::SeqCst),
    103 atomic_xadd => Atomic(AtomicOp::Xadd, AtomicOrders::Generic),
    104 atomic_xadd_acqrel => Atomic(AtomicOp::Xadd, AtomicOrders::AcqRel),
    105 atomic_xadd_acquire => Atomic(AtomicOp::Xadd, AtomicOrders::Acquire),
    106 atomic_xadd_relaxed => Atomic(AtomicOp::Xadd, AtomicOrders::Relaxed),
    107 atomic_xadd_release => Atomic(AtomicOp::Xadd, AtomicOrders::Release),
    108 atomic_xadd_seqcst => Atomic(AtomicOp::Xadd, AtomicOrders::SeqCst),
    109 atomic_xchg => Atomic(AtomicOp::Xchg, AtomicOrders::Generic),
    110 atomic_xchg_acqrel => Atomic(AtomicOp::Xchg, AtomicOrders::AcqRel),
    111 atomic_xchg_acquire => Atomic(AtomicOp::Xchg, AtomicOrders::Acquire),
    112 atomic_xchg_relaxed => Atomic(AtomicOp::Xchg, AtomicOrders::Relaxed),
    113 atomic_xchg_release => Atomic(AtomicOp::Xchg, AtomicOrders::Release),
    114 atomic_xchg_seqcst => Atomic(AtomicOp::Xchg, AtomicOrders::SeqCst),
    115 atomic_xor => Atomic(AtomicOp::Xor, AtomicOrders::Generic),
    116 atomic_xor_acqrel => Atomic(AtomicOp::Xor, AtomicOrders::AcqRel),
    117 atomic_xor_acquire => Atomic(AtomicOp::Xor, AtomicOrders::Acquire),
    118 atomic_xor_relaxed => Atomic(AtomicOp::Xor, AtomicOrders::Relaxed),
    119 atomic_xor_release => Atomic(AtomicOp::Xor, AtomicOrders::Release),
    120 atomic_xor_seqcst => Atomic(AtomicOp::Xor, AtomicOrders::SeqCst),
    121 atomic_xsub => Atomic(AtomicOp::Xsub, AtomicOrders::Generic),
    122 atomic_xsub_acqrel => Atomic(AtomicOp::Xsub, AtomicOrders::AcqRel),
    123 atomic_xsub_acquire => Atomic(AtomicOp::Xsub, AtomicOrders::Acquire),
    124 atomic_xsub_relaxed => Atomic(AtomicOp::Xsub, AtomicOrders::Relaxed),
    125 atomic_xsub_release => Atomic(AtomicOp::Xsub, AtomicOrders::Release),
    126 atomic_xsub_seqcst => Atomic(AtomicOp::Xsub, AtomicOrders::SeqCst),
    127 autodiff => Autodiff,
    128 bitreverse => Bitreverse,
    129 black_box => BlackBox,
    130 box_new => BoxNew,
    131 breakpoint => Breakpoint,
    132 bswap => Bswap,
    133 caller_location => CallerLocation,
    134 carrying_mul_add => CarryingMulAdd,
    135 carryless_mul => CarrylessMul,
    136 catch_unwind => CatchUnwind,
    137 ceil => Ceil,
    138 codeview_annotation => CodeviewAnnotation,
    139 cold_path => ColdPath,
    140 compare_bytes => CompareBytes,
    141 const_allocate => ConstAllocate,
    142 const_deallocate => ConstDeallocate,
    143 const_eval_select => ConstEvalSelect,
    144 const_make_global => ConstMakeGlobal,
    145 contract_check_ensures => ContractCheckEnsures,
    146 contract_check_requires => ContractCheckRequires,
    147 copy => Copy,
    148 copy_nonoverlapping => CopyNonoverlapping,
    149 copysignf128 => CopySign(FloatWidth::F128),
    150 copysignf16 => CopySign(FloatWidth::F16),
    151 copysignf32 => CopySign(FloatWidth::F32),
    152 copysignf64 => CopySign(FloatWidth::F64),
    153 cos => Cos,
    154 ctlz => Ctlz,
    155 ctlz_nonzero => CtlzNonzero,
    156 ctpop => Ctpop,
    157 cttz => Cttz,
    158 cttz_nonzero => CttzNonzero,
    159 discriminant_value => DiscriminantValue,
    160 disjoint_bitor => DisjointBitor,
    161 exact_div => ExactDiv,
    162 exp => Exp,
    163 exp2 => Exp2,
    164 fabs => Fabs,
    165 fadd_algebraic => FaddAlgebraic,
    166 fadd_fast => FaddFast,
    167 fdiv_algebraic => FdivAlgebraic,
    168 fdiv_fast => FdivFast,
    169 field_offset => FieldOffset,
    170 field_representing_type_actual_type_id => FieldRepresentingTypeActualTypeId,
    171 field_representing_type_name => FieldRepresentingTypeName,
    172 field_representing_type_offset => FieldRepresentingTypeOffset,
    173 float_to_int_unchecked => FloatToIntUnchecked,
    174 floor => Floor,
    175 fma => Fma,
    176 fmul_algebraic => FmulAlgebraic,
    177 fmul_fast => FmulFast,
    178 fmuladd => Fmuladd,
    179 forget => Forget,
    180 frem_algebraic => FremAlgebraic,
    181 frem_fast => FremFast,
    182 fsub_algebraic => FsubAlgebraic,
    183 fsub_fast => FsubFast,
    184 gpu_launch_sized_workgroup_mem => GpuLaunchSizedWorkgroupMem,
    185 integer_max => IntegerMax,
    186 integer_min => IntegerMin,
    187 is_val_statically_known => IsValStaticallyKnown,
    188 log => Log,
    189 log10 => Log10,
    190 log2 => Log2,
    191 maximum_number_nsz_f128 => MaximumNumberNsz(FloatWidth::F128),
    192 maximum_number_nsz_f16 => MaximumNumberNsz(FloatWidth::F16),
    193 maximum_number_nsz_f32 => MaximumNumberNsz(FloatWidth::F32),
    194 maximum_number_nsz_f64 => MaximumNumberNsz(FloatWidth::F64),
    195 maximumf128 => Maximum(FloatWidth::F128),
    196 maximumf16 => Maximum(FloatWidth::F16),
    197 maximumf32 => Maximum(FloatWidth::F32),
    198 maximumf64 => Maximum(FloatWidth::F64),
    199 minimum_number_nsz_f128 => MinimumNumberNsz(FloatWidth::F128),
    200 minimum_number_nsz_f16 => MinimumNumberNsz(FloatWidth::F16),
    201 minimum_number_nsz_f32 => MinimumNumberNsz(FloatWidth::F32),
    202 minimum_number_nsz_f64 => MinimumNumberNsz(FloatWidth::F64),
    203 minimumf128 => Minimum(FloatWidth::F128),
    204 minimumf16 => Minimum(FloatWidth::F16),
    205 minimumf32 => Minimum(FloatWidth::F32),
    206 minimumf64 => Minimum(FloatWidth::F64),
    207 mul_with_overflow => MulWithOverflow,
    208 needs_drop => NeedsDrop,
    209 nontemporal_store => NontemporalStore,
    210 offload => Offload,
    211 offload_get_num_devices => OffloadGetNumDevices,
    212 offset => Offset,
    213 offset_of => OffsetOf,
    214 overflow_checks => OverflowChecks,
    215 powf => Powf,
    216 powi => Powi,
    217 prefetch_read_data => PrefetchReadData,
    218 prefetch_read_instruction => PrefetchReadInstruction,
    219 prefetch_write_data => PrefetchWriteData,
    220 prefetch_write_instruction => PrefetchWriteInstruction,
    221 ptr_guaranteed_cmp => PtrGuaranteedCmp,
    222 ptr_mask => PtrMask,
    223 ptr_metadata => PtrMetadata,
    224 ptr_offset_from => PtrOffsetFrom,
    225 ptr_offset_from_unsigned => PtrOffsetFromUnsigned,
    226 raw_eq => RawEq,
    227 read_via_copy => ReadViaCopy,
    228 return_address => ReturnAddress,
    229 rotate_left => RotateLeft,
    230 rotate_right => RotateRight,
    231 round => Round,
    232 round_ties_even => RoundTiesEven,
    233 rustc_peek => RustcPeek,
    234 saturating_add => SaturatingAdd,
    235 saturating_sub => SaturatingSub,
    236 select_unpredictable => SelectUnpredictable,
    237 simd_add => Simd(SimdOp::Add),
    238 simd_and => Simd(SimdOp::And),
    239 simd_arith_offset => Simd(SimdOp::ArithOffset),
    240 simd_as => Simd(SimdOp::As),
    241 simd_bitmask => Simd(SimdOp::Bitmask),
    242 simd_bitreverse => Simd(SimdOp::Bitreverse),
    243 simd_bswap => Simd(SimdOp::Bswap),
    244 simd_carryless_mul => Simd(SimdOp::CarrylessMul),
    245 simd_cast => Simd(SimdOp::Cast),
    246 simd_cast_ptr => Simd(SimdOp::CastPtr),
    247 simd_ceil => Simd(SimdOp::Ceil),
    248 simd_ctlz => Simd(SimdOp::Ctlz),
    249 simd_ctpop => Simd(SimdOp::Ctpop),
    250 simd_cttz => Simd(SimdOp::Cttz),
    251 simd_div => Simd(SimdOp::Div),
    252 simd_eq => Simd(SimdOp::Eq),
    253 simd_expose_provenance => Simd(SimdOp::ExposeProvenance),
    254 simd_extract => Simd(SimdOp::Extract),
    255 simd_extract_dyn => Simd(SimdOp::ExtractDyn),
    256 simd_fabs => Simd(SimdOp::Fabs),
    257 simd_fcos => Simd(SimdOp::Fcos),
    258 simd_fexp => Simd(SimdOp::Fexp),
    259 simd_fexp2 => Simd(SimdOp::Fexp2),
    260 simd_flog => Simd(SimdOp::Flog),
    261 simd_flog10 => Simd(SimdOp::Flog10),
    262 simd_flog2 => Simd(SimdOp::Flog2),
    263 simd_floor => Simd(SimdOp::Floor),
    264 simd_fma => Simd(SimdOp::Fma),
    265 simd_fsin => Simd(SimdOp::Fsin),
    266 simd_fsqrt => Simd(SimdOp::Fsqrt),
    267 simd_funnel_shl => Simd(SimdOp::FunnelShl),
    268 simd_funnel_shr => Simd(SimdOp::FunnelShr),
    269 simd_gather => Simd(SimdOp::Gather),
    270 simd_ge => Simd(SimdOp::Ge),
    271 simd_gt => Simd(SimdOp::Gt),
    272 simd_insert => Simd(SimdOp::Insert),
    273 simd_insert_dyn => Simd(SimdOp::InsertDyn),
    274 simd_le => Simd(SimdOp::Le),
    275 simd_lt => Simd(SimdOp::Lt),
    276 simd_masked_load => Simd(SimdOp::MaskedLoad),
    277 simd_masked_store => Simd(SimdOp::MaskedStore),
    278 simd_maximum_number_nsz => Simd(SimdOp::MaximumNumberNsz),
    279 simd_minimum_number_nsz => Simd(SimdOp::MinimumNumberNsz),
    280 simd_mul => Simd(SimdOp::Mul),
    281 simd_ne => Simd(SimdOp::Ne),
    282 simd_neg => Simd(SimdOp::Neg),
    283 simd_or => Simd(SimdOp::Or),
    284 simd_reduce_add_ordered => Simd(SimdOp::ReduceAddOrdered),
    285 simd_reduce_add_unordered => Simd(SimdOp::ReduceAddUnordered),
    286 simd_reduce_all => Simd(SimdOp::ReduceAll),
    287 simd_reduce_and => Simd(SimdOp::ReduceAnd),
    288 simd_reduce_any => Simd(SimdOp::ReduceAny),
    289 simd_reduce_max => Simd(SimdOp::ReduceMax),
    290 simd_reduce_min => Simd(SimdOp::ReduceMin),
    291 simd_reduce_mul_ordered => Simd(SimdOp::ReduceMulOrdered),
    292 simd_reduce_mul_unordered => Simd(SimdOp::ReduceMulUnordered),
    293 simd_reduce_or => Simd(SimdOp::ReduceOr),
    294 simd_reduce_xor => Simd(SimdOp::ReduceXor),
    295 simd_relaxed_fma => Simd(SimdOp::RelaxedFma),
    296 simd_rem => Simd(SimdOp::Rem),
    297 simd_round => Simd(SimdOp::Round),
    298 simd_round_ties_even => Simd(SimdOp::RoundTiesEven),
    299 simd_saturating_add => Simd(SimdOp::SaturatingAdd),
    300 simd_saturating_sub => Simd(SimdOp::SaturatingSub),
    301 simd_scatter => Simd(SimdOp::Scatter),
    302 simd_select => Simd(SimdOp::Select),
    303 simd_select_bitmask => Simd(SimdOp::SelectBitmask),
    304 simd_shl => Simd(SimdOp::Shl),
    305 simd_shr => Simd(SimdOp::Shr),
    306 simd_shuffle => Simd(SimdOp::Shuffle),
    307 simd_splat => Simd(SimdOp::Splat),
    308 simd_sub => Simd(SimdOp::Sub),
    309 simd_trunc => Simd(SimdOp::Trunc),
    310 simd_with_exposed_provenance => Simd(SimdOp::WithExposedProvenance),
    311 simd_xor => Simd(SimdOp::Xor),
    312 sin => Sin,
    313 size_of => SizeOf,
    314 size_of_val => SizeOfVal,
    315 slice_get_unchecked => SliceGetUnchecked,
    316 sqrt => Sqrt,
    317 sub_with_overflow => SubWithOverflow,
    318 sve_tuple_create2 => SveTupleCreate2,
    319 sve_tuple_create3 => SveTupleCreate3,
    320 sve_tuple_create4 => SveTupleCreate4,
    321 sve_tuple_get => SveTupleGet,
    322 sve_tuple_set => SveTupleSet,
    323 three_way_compare => ThreeWayCompare,
    324 transmute => Transmute,
    325 transmute_unchecked => TransmuteUnchecked,
    326 trunc => Trunc,
    327 type_id => TypeId,
    328 type_id_array_len => TypeIdArrayLen,
    329 type_id_element_ty => TypeIdElementTy,
    330 type_id_eq => TypeIdEq,
    331 type_id_field_representing_type => TypeIdFieldRepresentingType,
    332 type_id_fields => TypeIdFields,
    333 type_id_function_ptr => TypeIdFunctionPtr,
    334 type_id_generics => TypeIdGenerics,
    335 type_id_is_signed => TypeIdIsSigned,
    336 type_id_non_exhaustive => TypeIdNonExhaustive,
    337 type_id_points_mutably => TypeIdPointsMutably,
    338 type_id_points_to => TypeIdPointsTo,
    339 type_id_size_of => TypeIdSizeOf,
    340 type_id_type_of => TypeIdTypeOf,
    341 type_id_variant_name => TypeIdVariantName,
    342 type_id_variant_non_exhaustive => TypeIdVariantNonExhaustive,
    343 type_id_variants => TypeIdVariants,
    344 type_id_vtable => TypeIdVtable,
    345 type_name => TypeName,
    346 typed_swap_nonoverlapping => TypedSwapNonoverlapping,
    347 ub_checks => UbChecks,
    348 unaligned_volatile_load => UnalignedVolatileLoad,
    349 unaligned_volatile_store => UnalignedVolatileStore,
    350 unchecked_add => UncheckedAdd,
    351 unchecked_div => UncheckedDiv,
    352 unchecked_funnel_shl => UncheckedFunnelShl,
    353 unchecked_funnel_shr => UncheckedFunnelShr,
    354 unchecked_mul => UncheckedMul,
    355 unchecked_rem => UncheckedRem,
    356 unchecked_shl => UncheckedShl,
    357 unchecked_shr => UncheckedShr,
    358 unchecked_sub => UncheckedSub,
    359 unreachable => Unreachable,
    360 va_arg => VaArg,
    361 va_copy => VaCopy,
    362 va_end => VaEnd,
    363 variant_count => VariantCount,
    364 volatile_copy_memory => VolatileCopyMemory,
    365 volatile_copy_nonoverlapping_memory => VolatileCopyNonoverlappingMemory,
    366 volatile_load => VolatileLoad,
    367 volatile_set_memory => VolatileSetMemory,
    368 volatile_store => VolatileStore,
    369 vtable_align => VtableAlign,
    370 vtable_size => VtableSize,
    371 wrapping_add => WrappingAdd,
    372 wrapping_mul => WrappingMul,
    373 wrapping_sub => WrappingSub,
    374 write_box_via_move => WriteBoxViaMove,
    375 write_bytes => WriteBytes,
    376 write_via_move => WriteViaMove,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_name_is_its_own_intrinsic() {
        assert_eq!(Intrinsic::COUNT, 377);
        assert_eq!(BY_INDEX.iter().filter(|intrinsic| !intrinsic.is_legacy()).count(), 277);
        let mut seen = [false; Intrinsic::COUNT];
        for (at, intrinsic) in BY_INDEX.into_iter().enumerate() {
            assert_eq!(usize::from(intrinsic.index()), at, "{intrinsic}");
            assert!(!std::mem::replace(&mut seen[at], true));
            let name = intrinsic.symbol();
            assert_eq!(Intrinsic::from_symbol(&name), Some(intrinsic));
            assert_eq!(Intrinsic::from_symbol(&Symbol::intern(name.as_str())), Some(intrinsic));
            assert_eq!(Intrinsic::from_index(intrinsic.index()), Some(intrinsic));
        }
        assert_eq!(Intrinsic::from_index(Intrinsic::COUNT as u16), None);
    }

    #[test]
    fn names_are_in_byte_order() {
        for pair in BY_INDEX.windows(2) {
            let (a, b) = (pair[0].symbol(), pair[1].symbol());
            assert!(a.as_str().as_bytes() < b.as_str().as_bytes(), "{a} before {b}");
        }
    }

    #[test]
    fn families_carry_their_operation() {
        let of = |name: &str| Intrinsic::from_symbol(&Symbol::intern(name));
        let generic = |op| Some(Intrinsic::Atomic(op, AtomicOrders::Generic));
        assert_eq!(of("atomic_load"), generic(AtomicOp::Load));
        assert_eq!(of("atomic_cxchgweak"), generic(AtomicOp::CxchgWeak));
        assert_eq!(of("atomic_singlethreadfence"), generic(AtomicOp::SingleThreadFence));
        assert_eq!(of("simd_add"), Some(Intrinsic::Simd(SimdOp::Add)));
        assert_eq!(of("simd_reduce_add_ordered"), Some(Intrinsic::Simd(SimdOp::ReduceAddOrdered)));
        assert_eq!(of("simd_shuffle"), Some(Intrinsic::Simd(SimdOp::Shuffle)));
        assert_eq!(of("copysignf128"), Some(Intrinsic::CopySign(FloatWidth::F128)));
        assert_eq!(of("minimumf16"), Some(Intrinsic::Minimum(FloatWidth::F16)));
        assert_eq!(
            of("maximum_number_nsz_f64"),
            Some(Intrinsic::MaximumNumberNsz(FloatWidth::F64))
        );
        assert_eq!(of("sqrt"), Some(Intrinsic::Sqrt));
        assert_eq!(of("transmute"), Some(Intrinsic::Transmute));
        assert_eq!(of("write_box_via_move"), Some(Intrinsic::WriteBoxViaMove));
        assert_eq!(of("type_id_vtable"), Some(Intrinsic::TypeIdVtable));
        assert_eq!(Intrinsic::Simd(SimdOp::Eq).to_string(), "simd_eq");
        assert_eq!(
            Intrinsic::Atomic(AtomicOp::Xsub, AtomicOrders::Generic).to_string(),
            "atomic_xsub"
        );
        assert_eq!(
            Intrinsic::Atomic(AtomicOp::Cxchg, AtomicOrders::SeqCstAcquire).to_string(),
            "atomic_cxchg_seqcst_acquire"
        );
        assert_eq!(FloatWidth::F128.bits(), 128);
    }

    #[test]
    fn older_names_carry_their_orderings() {
        use AtomicOrdering::*;
        let of = |name: &str| {
            let Some(Intrinsic::Atomic(op, orders)) = Intrinsic::from_symbol(&Symbol::intern(name))
            else {
                panic!("{name} is not an atomic");
            };
            (op, orders.named())
        };
        // The ones `hir-ty-ub`'s fixtures call.
        assert_eq!(of("atomic_load_acquire"), (AtomicOp::Load, Some((Acquire, None))));
        assert_eq!(of("atomic_load_relaxed"), (AtomicOp::Load, Some((Relaxed, None))));
        assert_eq!(of("atomic_store_release"), (AtomicOp::Store, Some((Release, None))));
        assert_eq!(of("atomic_store_relaxed"), (AtomicOp::Store, Some((Relaxed, None))));
        assert_eq!(of("atomic_xadd_seqcst"), (AtomicOp::Xadd, Some((SeqCst, None))));
        assert_eq!(of("atomic_fence_acqrel"), (AtomicOp::Fence, Some((AcqRel, None))));
        assert_eq!(
            of("atomic_cxchg_acqrel_relaxed"),
            (AtomicOp::Cxchg, Some((AcqRel, Some(Relaxed))))
        );
        assert_eq!(
            of("atomic_cxchgweak_release_seqcst"),
            (AtomicOp::CxchgWeak, Some((Release, Some(SeqCst))))
        );
        assert_eq!(of("atomic_load"), (AtomicOp::Load, None));
        for intrinsic in BY_INDEX {
            match intrinsic {
                Intrinsic::Atomic(op, orders) => {
                    assert_eq!(intrinsic.is_legacy(), orders.named().is_some(), "{intrinsic}");
                    if let Some((_, failure)) = orders.named() {
                        let cxchg = matches!(op, AtomicOp::Cxchg | AtomicOp::CxchgWeak);
                        assert_eq!(failure.is_some(), cxchg, "{intrinsic}");
                    }
                }
                Intrinsic::BoxNew => assert!(intrinsic.is_legacy()),
                _ => assert!(!intrinsic.is_legacy(), "{intrinsic}"),
            }
        }
        assert_eq!(Intrinsic::from_symbol(&Symbol::intern("box_new")), Some(Intrinsic::BoxNew));
        for (name, intrinsic) in [
            ("offset", Intrinsic::Offset),
            ("arith_offset", Intrinsic::ArithOffset),
            ("transmute", Intrinsic::Transmute),
        ] {
            assert_eq!(Intrinsic::from_symbol(&Symbol::intern(name)), Some(intrinsic));
        }
    }

    #[test]
    fn orderings_are_cores_discriminants() {
        use AtomicOrdering::*;
        for ordering in [Relaxed, Release, Acquire, AcqRel, SeqCst] {
            assert_eq!(AtomicOrdering::from_discriminant(ordering as u128), Some(ordering));
        }
        assert_eq!(AtomicOrdering::from_discriminant(5), None);
    }

    #[test]
    fn names_shared_with_other_symbols_are_intrinsics() {
        assert_eq!(Intrinsic::from_symbol(&sym::copy), Some(Intrinsic::Copy));
        assert_eq!(Intrinsic::from_symbol(&sym::type_id), Some(Intrinsic::TypeId));
        assert_eq!(Intrinsic::from_symbol(&sym::unreachable), Some(Intrinsic::Unreachable));
        assert_eq!(Intrinsic::from_symbol(&sym::overflow_checks), Some(Intrinsic::OverflowChecks));
    }

    #[test]
    fn names_rustc_does_not_have_are_none() {
        // Older names not kept, and declared symbols that are not intrinsics.
        for name in [
            "sqrtf32",
            "atomic_load_unordered",
            "atomic_store_unordered",
            "atomic_cxchg_seqcst",
            "likely",
            "unlikely",
            "abort",
            "simd_fmin",
            "min_align_of",
            "simd_",
            "atomic_",
            "",
            "self",
            "Copy",
        ] {
            assert_eq!(Intrinsic::from_symbol(&Symbol::intern(name)), None, "{name}");
        }
        assert_eq!(Intrinsic::from_symbol(&sym::self_), None);
        assert_eq!(Intrinsic::from_symbol(&sym::Integer::get(1)), None);
    }

    #[test]
    fn unknown_intrinsics_name_themselves() {
        let message = UnknownIntrinsic(Symbol::intern("sqrtf32")).to_string();
        assert!(message.starts_with("`sqrtf32` is declared `#[rustc_intrinsic]`"), "{message}");
    }
}
