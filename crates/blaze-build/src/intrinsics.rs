//! The intrinsics table: every compiler intrinsic blaze builds in place, each with its meaning
//! quoted from the pinned `library/core/src/intrinsics/mod.rs` and the LIR it becomes
//! ([`TABLE`]). A name not in the table is refused by name ([`UnknownIntrinsic`]), never
//! guessed and never called as a function.
//!
//! # The seam
//!
//! The walk evaluates a call's arguments, then calls [`build`] with the intrinsic, its generic
//! arguments (types as ground [`PTy`]s, const generics as their values: an `AtomicOrdering` as
//! its discriminant, a `bool` as 0 or 1), the arguments as the walk holds them ([`Arg`]: a
//! scalar, or the address of an aggregate), and the call's type. [`build`] appends LIR to the
//! walk's builder and says where the result is ([`Out`]).
//!
//! # Values
//!
//! A scalar is the LIR type of its layout's primitive (`bool` an `i8` of 0 or 1, as the walk
//! holds it); an aggregate (a layout that is not one scalar, a wide pointer included) is an
//! address. A `(T, bool)` result is [`Out::Pair`]: the walk stores it by the tuple's layout.
//!
//! # Atomics
//!
//! An atomic intrinsic becomes the LIR atomic operation of the same meaning with its ordering:
//! core's `AtomicOrdering` (Relaxed 0, Release 1, Acquire 2, AcqRel 3, SeqCst 4) to LIR's
//! `Ordering`. The AArch64 instructions are the emitter's choice, by whether the target has
//! LSE (ARMv8.1 atomics; every Apple arm64 core has them), a value the emitter is given: with
//! LSE, `ldadd`/`ldclr`/`ldeor`/`ldset`/`swp`/`ldsmax`/`ldumin`... and `cas`, each with its
//! `a`/`l`/`al` suffix from the ordering; without, an `ldaxr`/`stlxr` loop; a fence is `dmb
//! ish` (`dmb ishld` for an acquire fence), a single-thread fence nothing but a compiler
//! barrier. A pointer-typed atomic computes on the address (`expose`, then `from_exposed`).
//!
//! # Hints
//!
//! `likely`, `unlikely` and `black_box` return their argument (`black_box` is "best effort";
//! the baseline tier optimises nothing away). `assume` is LIR `assume`. `cold_path` builds
//! nothing: the walk may mark the branch that reaches it.

use core::alloc::Allocator;
use core::fmt;

use blaze_lir::builder::{DataReloc, MemAttrs};
use blaze_lir::func::SlotKind;
use blaze_lir::{AtomicRmw, Builder, Flags, IntCond, Op, Ordering, Sink, Ty, Value};
use blaze_typeck::{PNode, PTy};

use crate::layout::{LayoutCx, LayoutError, LayoutId, Layouts, Prim, Tail, TagEncoding, Variants};

/// The trap code of `abort_immediate`: `brk #0xfe00` on AArch64.
pub const ABORT_TRAP: u32 = 0xfe00;

/// An intrinsic of the table.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Which {
    SizeOf,
    AlignOf,
    SizeOfVal,
    AlignOfVal,
    NeedsDrop,
    Forget,
    Transmute,
    TransmuteUnchecked,
    Copy,
    CopyNonoverlapping,
    WriteBytes,
    VolatileCopyMemory,
    VolatileCopyNonoverlappingMemory,
    VolatileSetMemory,
    ReadViaCopy,
    WriteViaMove,
    VolatileLoad,
    VolatileStore,
    UnalignedVolatileLoad,
    UnalignedVolatileStore,
    Offset,
    ArithOffset,
    PtrOffsetFrom,
    PtrOffsetFromUnsigned,
    WrappingAdd,
    WrappingSub,
    WrappingMul,
    AddWithOverflow,
    SubWithOverflow,
    MulWithOverflow,
    SaturatingAdd,
    SaturatingSub,
    UncheckedAdd,
    UncheckedSub,
    UncheckedMul,
    UncheckedDiv,
    UncheckedRem,
    ExactDiv,
    UncheckedShl,
    UncheckedShr,
    Ctpop,
    Ctlz,
    CtlzNonzero,
    Cttz,
    CttzNonzero,
    Bswap,
    Bitreverse,
    RotateLeft,
    RotateRight,
    Likely,
    Unlikely,
    Assume,
    ColdPath,
    /// alloc's `__rust_no_alloc_shim_is_unstable_v2`: no rustc intrinsic, a fn rustc's
    /// allocator shim generates with an empty body (blaze-ladder front2/alloc_shim.rs).
    AllocShimMarker,
    BlackBox,
    Unreachable,
    AbortImmediate,
    DiscriminantValue,
    AtomicLoad,
    AtomicStore,
    AtomicXchg,
    AtomicXadd,
    AtomicXsub,
    AtomicAnd,
    AtomicNand,
    AtomicOr,
    AtomicXor,
    AtomicMax,
    AtomicMin,
    AtomicUmax,
    AtomicUmin,
    AtomicCxchg,
    AtomicCxchgWeak,
    AtomicFence,
    AtomicSingleThreadFence,
    CallerLocation,
    PtrMetadata,
    AggregateRawPtr,
    ConstEvalSelect,
    /// Built by the walk (walk/unwind.rs): an `invoke` and a `catch` landing block.
    CatchUnwind,
    PtrGuaranteedCmp,
    ThreeWayCompare,
    SliceGetUnchecked,
    /// core's `ub_checks()`: `true` in a constant (blaze-consteval machine.rs `intrinsic`);
    /// built from its body `cfg!(ub_checks)` (front2 tables an intrinsic with a body by its body).
    UbChecks,
    /// A `simd_*` intrinsic: its rows and its build are intrinsics/simd.rs's.
    Simd(simd::SimdOp),
}

/// One row of the table.
pub struct Entry {
    pub name: &'static str,
    pub which: Which,
    /// The first sentence of its doc comment in the pinned core.
    pub meaning: &'static str,
    /// What [`build`] makes of it.
    pub built: &'static str,
    /// Which generic argument names the element a pointer-stepping intrinsic steps by, read
    /// from its signature in the pinned core (intrinsics/element.rs).
    pub element: ElementArg,
}

const fn e(name: &'static str, which: Which, meaning: &'static str, built: &'static str) -> Entry {
    Entry { name, which, meaning, built, element: ElementArg::None }
}

/// Every intrinsic blaze builds, sorted by name.
pub const TABLE: &[Entry] = &[
    e("__rust_no_alloc_shim_is_unstable_v2", Which::AllocShimMarker, "Generated by rustc's allocator shim with an empty body (alloc/src/alloc.rs declares it in `extern \"Rust\"`).", "nothing"),
    e("abort_immediate", Which::AbortImmediate, "Ungracefully aborts the execution of the process.", "`trap` ABORT_TRAP"),
    e("add_with_overflow", Which::AddWithOverflow, "Performs checked integer addition.", "`uadd_overflow`/`sadd_overflow`, the flag widened to a bool"),
    e("aggregate_raw_ptr", Which::AggregateRawPtr, "Lowers in MIR to `Rvalue::Aggregate` with `AggregateKind::RawPtr`.", "the data pointer for `()` metadata, else the two words of the wide pointer"),
    e("align_of", Which::AlignOf, "The minimum alignment of a type.", "a constant from the layout"),
    e("align_of_val", Which::AlignOfVal, "The required alignment of the referenced value.", "a constant: a slice tail's alignment is static; `dyn` refused"),
    e("arith_offset", Which::ArithOffset, "Calculates the offset from a pointer, potentially wrapping.", "`ptr_add` of `delta * size`, no promise").stepping(ElementArg::Is(0)),
    e("assume", Which::Assume, "Informs the optimizer that a condition is always true. If the condition is false, the behavior is undefined.", "`assume`"),
    e("atomic_and", Which::AtomicAnd, "Bitwise and with the current value, returning the previous value.", "`atomic_rmw and`"),
    e("atomic_cxchg", Which::AtomicCxchg, "Stores a value if the current value is the same as the `old` value, with the returned `T` being the previous value and the returned `bool` being whether the exchange was successful.", "`cmpxchg` with both orderings"),
    e("atomic_cxchgweak", Which::AtomicCxchgWeak, "Stores a value if the current value is the same as the `old` value, with the returned `T` being the previous value and the returned `bool` being whether the exchange was successful. The comparison may spuriously fail.", "`cmpxchg weak`"),
    e("atomic_fence", Which::AtomicFence, "An atomic fence.", "`fence`"),
    e("atomic_load", Which::AtomicLoad, "Loads the current value of the pointer.", "`atomic_load`, `volatile` when VOLATILE"),
    e("atomic_max", Which::AtomicMax, "Maximum with the current value using a signed comparison.", "`atomic_rmw max`"),
    e("atomic_min", Which::AtomicMin, "Minimum with the current value using a signed comparison.", "`atomic_rmw min`"),
    e("atomic_nand", Which::AtomicNand, "Bitwise nand with the current value, returning the previous value.", "`atomic_rmw nand`"),
    e("atomic_or", Which::AtomicOr, "Bitwise or with the current value, returning the previous value.", "`atomic_rmw or`"),
    e("atomic_singlethreadfence", Which::AtomicSingleThreadFence, "An atomic fence for synchronization within a single thread.", "`fence single_thread`"),
    e("atomic_store", Which::AtomicStore, "Stores the value at the specified memory location.", "`atomic_store`, `volatile` when VOLATILE"),
    e("atomic_umax", Which::AtomicUmax, "Maximum with the current value using an unsigned comparison.", "`atomic_rmw umax`"),
    e("atomic_umin", Which::AtomicUmin, "Minimum with the current value using an unsigned comparison.", "`atomic_rmw umin`"),
    e("atomic_xadd", Which::AtomicXadd, "Adds to the current value, returning the previous value.", "`atomic_rmw add`"),
    e("atomic_xchg", Which::AtomicXchg, "Stores the value at the specified memory location, returning the old value.", "`atomic_rmw xchg`"),
    e("atomic_xor", Which::AtomicXor, "Bitwise xor with the current value, returning the previous value.", "`atomic_rmw xor`"),
    e("atomic_xsub", Which::AtomicXsub, "Subtract from the current value, returning the previous value.", "`atomic_rmw sub`"),
    e("bitreverse", Which::Bitreverse, "Reverses the bits in an integer type `T`.", "`bitrev`"),
    e("black_box", Which::BlackBox, "See documentation of [`std::hint::black_box`] for details.", "its argument"),
    e("bswap", Which::Bswap, "Reverses the bytes in an integer type `T`.", "`bswap` (a byte: itself)"),
    e("caller_location", Which::CallerLocation, "Gets a reference to a static `Location` indicating where it was called.", "the address of a data row of the site's `Location`, or the location a `#[track_caller]` fn was passed"),
    e("catch_unwind", Which::CatchUnwind, "Rust's \"try catch\" construct for unwinding.", "an `invoke` of `try_fn` whose `catch` landing block calls `catch_fn` (walk/unwind.rs); under `panic = abort` a call and `false`"),
    e("cold_path", Which::ColdPath, "Hints to the compiler that current code path is cold.", "nothing"),
    e("const_eval_select", Which::ConstEvalSelect, "Selects which function to call depending on the context.", "the walk calls the run-time fn with the tuple's fields; the constant evaluator calls the const fn"),
    e("copy", Which::Copy, "Copies `count * size_of::<T>()` bytes from `src` to `dst`. The source and destination may overlap.", "`memmove` of `count * size`"),
    e("copy_nonoverlapping", Which::CopyNonoverlapping, "Copies `count * size_of::<T>()` bytes from `src` to `dst`. The source and destination must *not* overlap.", "`memcpy` of `count * size`"),
    e("ctlz", Which::Ctlz, "Returns the number of leading unset bits (zeroes) in an integer type `T`.", "`clz`, to `u32`"),
    e("ctlz_nonzero", Which::CtlzNonzero, "Like `ctlz`, but extra-unsafe as it returns `undef` when given an `x` with value `0`.", "`clz nonzero`, to `u32`"),
    e("ctpop", Which::Ctpop, "Returns the number of bits set in an integer type `T`", "`popcnt`, to `u32`"),
    e("cttz", Which::Cttz, "Returns the number of trailing unset bits (zeroes) in an integer type `T`.", "`ctz`, to `u32`"),
    e("cttz_nonzero", Which::CttzNonzero, "Like `cttz`, but extra-unsafe as it returns `undef` when given an `x` with value `0`.", "`ctz nonzero`, to `u32`"),
    e("discriminant_value", Which::DiscriminantValue, "Returns the value of the discriminant for the variant in 'v'; if `T` has no discriminant, returns `0`.", "the tag loaded and decoded by the layout's encoding, mapped to the discriminant"),
    e("exact_div", Which::ExactDiv, "Performs an exact division, resulting in undefined behavior where `x % y != 0` or `y == 0` or `x == T::MIN && y == -1`", "`sdiv`/`udiv exact`"),
    e("forget", Which::Forget, "Moves a value out of scope without running drop glue.", "nothing"),
    e("likely", Which::Likely, "Hints to the compiler that branch condition is likely to be true. Returns the value passed to it.", "its argument"),
    e("mul_with_overflow", Which::MulWithOverflow, "Performs checked integer multiplication", "`umul_overflow`/`smul_overflow`"),
    e("needs_drop", Which::NeedsDrop, "Returns `true` if the actual type given as `T` requires drop glue; returns `false` if the actual type provided for `T` implements `Copy`.", "a constant from the drop facts"),
    e("offset", Which::Offset, "Calculates the offset from a pointer.", "`ptr_add inbounds` of `delta * size`").stepping(ElementArg::PointeeOf(0)),
    e("ptr_guaranteed_cmp", Which::PtrGuaranteedCmp, "See documentation of `<*const T>::guaranteed_eq` for details.", "`icmp eq` zero-extended to `u8` (never 2 at run time); wide pointers refused by name; the constant evaluator answers 2 (unknown) unless certain"),
    e("ptr_metadata", Which::PtrMetadata, "Lowers in MIR to `Rvalue::UnaryOp` with `UnOp::PtrMetadata`.", "a wide pointer's word at 8, read as `M`; `()` for a thin pointer"),
    e("ptr_offset_from", Which::PtrOffsetFrom, "See documentation of `<*const T>::offset_from` for details.", "the address difference `sdiv exact` the size"),
    e("ptr_offset_from_unsigned", Which::PtrOffsetFromUnsigned, "See documentation of `<*const T>::offset_from_unsigned` for details.", "the address difference `udiv exact` the size"),
    e("read_via_copy", Which::ReadViaCopy, "This is an implementation detail of [`crate::ptr::read`] and should not be used anywhere else.", "a `load` of a scalar; an aggregate is the `T` at the pointer"),
    e("rotate_left", Which::RotateLeft, "Performs rotate left.", "`rotl`, the amount at `T`'s width"),
    e("rotate_right", Which::RotateRight, "Performs rotate right.", "`rotr`, the amount at `T`'s width"),
    e("saturating_add", Which::SaturatingAdd, "Computes `a + b`, saturating at numeric bounds.", "`uadd_sat`/`sadd_sat`"),
    e("saturating_sub", Which::SaturatingSub, "Computes `a - b`, saturating at numeric bounds.", "`usub_sat`/`ssub_sat`"),
    e("size_of", Which::SizeOf, "The size of a type in bytes.", "a constant from the layout"),
    e("size_of_val", Which::SizeOfVal, "The size of the referenced value in bytes.", "a constant, or the slice length times the stride plus the prefix, rounded; of `dyn`, the vtable's word 1"),
    e("slice_get_unchecked", Which::SliceGetUnchecked, "Projects to the `index`-th element of `slice_ptr`, as the same kind of pointer as the slice was provided -- so `&mut [T] → &mut T`, `&[T] → &T`, `*mut [T] → *mut T`, or `*const [T] → *const T` -- without a bounds check.", "the data word, `ptr_add inbounds` of `index * size_of::<T>()`; a zero-sized `T`: the data word"),
    e("sub_with_overflow", Which::SubWithOverflow, "Performs checked integer subtraction", "`usub_overflow`/`ssub_overflow`"),
    e("three_way_compare", Which::ThreeWayCompare, "Does a three-way comparison between the two arguments, which must be of character or integer (signed or unsigned) type.", "two `icmp`s (`slt`/`sgt`, or `ult`/`ugt` for unsigned and `char`) and two `select`s to `Ordering`'s -1/0/1"),
    e("transmute", Which::Transmute, "Reinterprets the bits of a value of one type as another type.", "the bits kept (`expose`/`from_exposed`/`bitcast` between scalar kinds); an aggregate through memory"),
    e("transmute_unchecked", Which::TransmuteUnchecked, "Like [`transmute`], but even less checked at compile-time: rather than giving an error for `size_of::<Src>() != size_of::<Dst>()`, it's **Undefined Behavior** at runtime.", "as `transmute`; sizes must still agree here"),
    e("ub_checks", Which::UbChecks, "Returns whether we should perform some UB-checking at runtime.", "its body, `cfg!(ub_checks)`; `true` in a constant"),
    e("unaligned_volatile_load", Which::UnalignedVolatileLoad, "Performs a volatile load from the `src` pointer The pointer is not required to be aligned.", "`load volatile`, alignment 1"),
    e("unaligned_volatile_store", Which::UnalignedVolatileStore, "Performs a volatile store to the `dst` pointer. The pointer is not required to be aligned.", "`store volatile`, alignment 1"),
    e("unchecked_add", Which::UncheckedAdd, "Returns the result of an unchecked addition, resulting in undefined behavior when `x + y > T::MAX` or `x + y < T::MIN`.", "`add nsw`/`add nuw`"),
    e("unchecked_div", Which::UncheckedDiv, "Performs an unchecked division, resulting in undefined behavior where `y == 0` or `x == T::MIN && y == -1`", "`sdiv`/`udiv`"),
    e("unchecked_mul", Which::UncheckedMul, "Returns the result of an unchecked multiplication, resulting in undefined behavior when `x * y > T::MAX` or `x * y < T::MIN`.", "`mul nsw`/`mul nuw`"),
    e("unchecked_rem", Which::UncheckedRem, "Returns the remainder of an unchecked division, resulting in undefined behavior when `y == 0` or `x == T::MIN && y == -1`", "`srem`/`urem`"),
    e("unchecked_shl", Which::UncheckedShl, "Performs an unchecked left shift, resulting in undefined behavior when `y < 0` or `y >= N`, where N is the width of T in bits.", "`shl in_range`, the amount at `T`'s width"),
    e("unchecked_shr", Which::UncheckedShr, "Performs an unchecked right shift, resulting in undefined behavior when `y < 0` or `y >= N`, where N is the width of T in bits.", "`ashr`/`lshr in_range`"),
    e("unchecked_sub", Which::UncheckedSub, "Returns the result of an unchecked subtraction, resulting in undefined behavior when `x - y > T::MAX` or `x - y < T::MIN`.", "`sub nsw`/`sub nuw`"),
    e("unlikely", Which::Unlikely, "Hints to the compiler that branch condition is likely to be false. Returns the value passed to it.", "its argument"),
    e("unreachable", Which::Unreachable, "Informs the optimizer that this point in the code is not reachable, enabling further optimizations.", "`unreachable`"),
    e("volatile_copy_memory", Which::VolatileCopyMemory, "Equivalent to the appropriate `llvm.memmove.p0i8.0i8.*` intrinsic, with a size of `count * size_of::<T>()` and an alignment of `align_of::<T>()`.", "`memmove volatile`"),
    e("volatile_copy_nonoverlapping_memory", Which::VolatileCopyNonoverlappingMemory, "Equivalent to the appropriate `llvm.memcpy.p0i8.0i8.*` intrinsic, with a size of `count` * `size_of::<T>()` and an alignment of `align_of::<T>()`.", "`memcpy volatile`"),
    e("volatile_load", Which::VolatileLoad, "Performs a volatile load from the `src` pointer.", "`load volatile`"),
    e("volatile_set_memory", Which::VolatileSetMemory, "Equivalent to the appropriate `llvm.memset.p0i8.*` intrinsic, with a size of `count * size_of::<T>()` and an alignment of `align_of::<T>()`.", "`memset volatile`"),
    e("volatile_store", Which::VolatileStore, "Performs a volatile store to the `dst` pointer.", "`store volatile`"),
    e("wrapping_add", Which::WrappingAdd, "Wrapping (modular) addition. Computes `a + b`, wrapping around at the boundary of the type.", "`add`"),
    e("wrapping_mul", Which::WrappingMul, "Wrapping (modular) multiplication. Computes `a * b`, wrapping around at the boundary of the type.", "`mul`"),
    e("wrapping_sub", Which::WrappingSub, "Wrapping (modular) subtraction. Computes `a - b`, wrapping around at the boundary of the type.", "`sub`"),
    e("write_bytes", Which::WriteBytes, "Sets `count * size_of::<T>()` bytes of memory starting at `dst` to `val`.", "`memset` of `count * size`"),
    e("write_via_move", Which::WriteViaMove, "This is an implementation detail of [`crate::ptr::write`] and should not be used anywhere else.", "a `store` of a scalar; `memcpy` of an aggregate"),
];

/// An intrinsic name the table lacks: a finding that names it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct UnknownIntrinsic<'n> {
    pub name: &'n str,
}

impl fmt::Display for UnknownIntrinsic<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "intrinsic `{}` is not in blaze's intrinsics table (crates/blaze-build/src/intrinsics.rs)", self.name)
    }
}

impl core::error::Error for UnknownIntrinsic<'_> {}

impl Which {
    /// The table's row for `name` (an intrinsic fn's name, as declared).
    pub fn named(name: &str) -> Result<Which, UnknownIntrinsic<'_>> {
        match TABLE.binary_search_by(|e| e.name.cmp(name)) {
            Ok(i) => Ok(TABLE[i].which),
            // The `simd_*` rows are intrinsics/simd.rs's table.
            Err(_) => simd::named(name).ok_or(UnknownIntrinsic { name }),
        }
    }

    pub fn entry(self) -> &'static Entry {
        TABLE.iter().chain(simd::TABLE).find(|e| e.which == self).expect("every `Which` has its row (tested)")
    }
}

/// A generic argument of the call.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GenericArg {
    Ty(PTy),
    /// A const generic's value (an enum by its discriminant, `bool` 0 or 1).
    Const(u128),
}

/// An argument as the walk holds it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Arg {
    /// A scalar.
    Scalar(Value),
    /// The address of an aggregate (a wide pointer included).
    Place(Value),
    /// A zero-sized value.
    None,
}

/// Where the result is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Out {
    /// No value (`()`, or a zero-sized result).
    None,
    Scalar(Value),
    /// The two fields of a two-field result, each written at its offset: a `(T, bool)` (the
    /// bool as an `i8` of 0 or 1), or a wide pointer's address and metadata.
    Pair(Value, Value),
    /// The result is the value at this address: the walk copies it out by the call's layout.
    At(Value),
    /// The call does not return: the block has ended.
    Diverges,
}

/// Where `caller_location` is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Site<'s> {
    /// The intrinsic's own call site: the file's path as written in the record of the
    /// crate's source, the 1-based line and column.
    Here { file: &'s str, line: u32, col: u32 },
    /// Inside a `#[track_caller]` fn: the `&Location` it was passed.
    Passed(Value),
}

/// One intrinsic call.
pub struct Call<'c> {
    pub which: Which,
    pub generics: &'c [GenericArg],
    pub args: &'c [Arg],
    /// The call's type.
    pub ret: PTy,
    /// For `caller_location`: where, and `Location<'static>`'s ground type.
    pub site: Option<(Site<'c>, PTy)>,
}

/// What [`build`] reads besides the call.
pub struct IntrinsicCx<'c, 'g, 't, A: Allocator + Clone> {
    pub layouts: &'c mut Layouts<A>,
    pub cx: &'c mut LayoutCx<'g, 't>,
    /// Whether a ground type needs drop glue (the drop elaboration's answer).
    pub needs_drop: &'c dyn Fn(PTy) -> bool,
}

/// Why an intrinsic call was not built.
#[derive(Debug)]
pub enum IntrinsicError {
    /// The call's generic arguments or arguments are not of the intrinsic's signature.
    Shape { which: Which, why: &'static str },
    /// A type the intrinsic reads has no layout.
    Layout(LayoutError),
    /// A use blaze does not build yet, named.
    Unsupported { which: Which, why: &'static str },
}

impl fmt::Display for IntrinsicError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IntrinsicError::Shape { which, why } => write!(f, "intrinsic `{}`: {why}", which.entry().name),
            IntrinsicError::Layout(e) => write!(f, "an intrinsic's type: {e}"),
            IntrinsicError::Unsupported { which, why } => {
                write!(f, "intrinsic `{}`: blaze does not build {why} yet", which.entry().name)
            }
        }
    }
}

impl core::error::Error for IntrinsicError {}

impl From<LayoutError> for IntrinsicError {
    fn from(e: LayoutError) -> IntrinsicError {
        IntrinsicError::Layout(e)
    }
}

/// A scalar's LIR type and signedness.
pub(crate) fn lir(prim: Prim) -> (Ty, bool) {
    match prim {
        Prim::Int { bytes: 1, signed } => (Ty::I8, signed),
        Prim::Int { bytes: 2, signed } => (Ty::I16, signed),
        Prim::Int { bytes: 4, signed } => (Ty::I32, signed),
        Prim::Int { bytes: 8, signed } => (Ty::I64, signed),
        Prim::Int { signed, .. } => (Ty::I128, signed),
        Prim::F16 => (Ty::F16, true),
        Prim::F32 => (Ty::F32, true),
        Prim::F64 => (Ty::F64, true),
        Prim::F128 => (Ty::scalar(blaze_lir::Scalar::F128), true),
        Prim::Ptr => (Ty::PTR, false),
    }
}

/// Atomic ordering from core's `AtomicOrdering` discriminant.
fn ordering(v: u128) -> Option<Ordering> {
    Some(match v {
        0 => Ordering::Relaxed,
        1 => Ordering::Release,
        2 => Ordering::Acquire,
        3 => Ordering::AcqRel,
        4 => Ordering::SeqCst,
        _ => return None,
    })
}

struct B<'b, 'c, 'g, 't, LA: Allocator + Clone, S: Sink, A: Allocator + Clone> {
    b: &'b mut Builder<LA, S>,
    cx: &'b mut IntrinsicCx<'c, 'g, 't, A>,
    which: Which,
}

impl<LA: Allocator + Clone, S: Sink, A: Allocator + Clone> B<'_, '_, '_, '_, LA, S, A> {
    fn shape(&self, why: &'static str) -> IntrinsicError {
        IntrinsicError::Shape { which: self.which, why }
    }

    fn layout(&mut self, ty: PTy) -> Result<LayoutId, IntrinsicError> {
        Ok(self.cx.layouts.of(ty, self.cx.cx)?)
    }

    fn node(&self, ty: PTy) -> Option<PNode> {
        self.cx.cx.ground.view().get(ty).copied()
    }

    /// The pointee of a pointer or reference type.
    fn pointee(&self, ty: PTy) -> Result<PTy, IntrinsicError> {
        match self.node(ty) {
            Some(PNode::Ptr(_, t) | PNode::Ref(_, t)) => Ok(t),
            _ => Err(self.shape("a generic argument that should be a pointer is not one")),
        }
    }

    /// A type's scalar: its LIR type and signedness.
    fn scalar(&mut self, ty: PTy) -> Result<(Ty, bool), IntrinsicError> {
        let id = self.layout(ty)?;
        let l = self.cx.layouts.get(id);
        l.scalar.map(|s| lir(s.prim)).ok_or(self.shape("a type that should be a scalar is not one"))
    }

    fn usize(&mut self, v: u64) -> Value {
        self.b.iconst(Ty::I64, u128::from(v))
    }

    /// `v` converted to `to`'s width, zero- or sign-extended.
    fn resize(&mut self, v: Value, to: Ty, signed: bool) -> Value {
        let from = self.b.ty(v);
        let (fb, tb) = (from.bits(64), to.bits(64));
        if fb == tb {
            v
        } else if fb > tb {
            self.b.convert(Op::Trunc, Flags::NONE, to, v)
        } else {
            self.b.convert(if signed { Op::Sext } else { Op::Zext }, Flags::NONE, to, v)
        }
    }
}

fn ty_arg(c: &Call<'_>, i: usize) -> Option<PTy> {
    match c.generics.get(i) {
        Some(GenericArg::Ty(t)) => Some(*t),
        _ => None,
    }
}

fn const_arg(c: &Call<'_>, i: usize) -> Option<u128> {
    match c.generics.get(i) {
        Some(GenericArg::Const(v)) => Some(*v),
        _ => None,
    }
}

/// Builds intrinsic call `c` into `b`, its arguments already built.
pub fn build<LA: Allocator + Clone, S: Sink, A: Allocator + Clone>(
    b: &mut Builder<LA, S>,
    c: &Call<'_>,
    cx: &mut IntrinsicCx<'_, '_, '_, A>,
) -> Result<Out, IntrinsicError> {
    let mut s = B { b, cx, which: c.which };
    let t0 = ty_arg(c, 0);
    let scalar = |i: usize| match c.args.get(i) {
        Some(Arg::Scalar(v)) => Some(*v),
        _ => None,
    };
    let shape = || IntrinsicError::Shape { which: c.which, why: "the arguments are not of the intrinsic's signature" };
    let need_t = || IntrinsicError::Shape { which: c.which, why: "a missing type argument" };
    use Which as W;
    let out = match c.which {
        W::SizeOf | W::AlignOf => {
            let id = s.layout(t0.ok_or_else(need_t)?)?;
            let l = *s.cx.layouts.get(id);
            let v = if c.which == W::SizeOf { l.size } else { l.align_bytes() };
            Out::Scalar(s.usize(v))
        }
        W::SizeOfVal | W::AlignOfVal => {
            let id = s.layout(t0.ok_or_else(need_t)?)?;
            let l = *s.cx.layouts.get(id);
            match (l.tail, c.which) {
                // codegen.md "Trait objects": the vtable (the wide pointer's word at 8) holds the
                // size at word 1 and the alignment at word 2.
                (Tail::Dyn, _) => {
                    let vt = match c.args.first() {
                        Some(Arg::Place(at)) => s.b.load(Ty::PTR, Flags::NONE, *at, 8, MemAttrs::aligned(3)),
                        _ => return Err(shape()),
                    };
                    let word = if c.which == W::SizeOfVal { 8 } else { 16 };
                    Out::Scalar(s.b.load(Ty::I64, Flags::NONE, vt, word, MemAttrs::aligned(3)))
                }
                (_, W::AlignOfVal) => Out::Scalar(s.usize(l.align_bytes())),
                (Tail::Sized, _) => Out::Scalar(s.usize(l.size)),
                (Tail::Slice { stride }, _) => {
                    // The length is the wide pointer's metadata, at 8.
                    let len = match c.args.first() {
                        Some(Arg::Place(at)) => s.b.load(Ty::I64, Flags::NONE, *at, 8, MemAttrs::aligned(3)),
                        _ => return Err(shape()),
                    };
                    let stride = s.usize(stride);
                    let tail = s.b.binary(Op::Mul, Flags::NONE, len, stride);
                    let prefix = s.usize(l.size);
                    let end = s.b.binary(Op::Add, Flags::NONE, tail, prefix);
                    let mask = (1u64 << l.align) - 1;
                    let m = s.usize(mask);
                    let up = s.b.binary(Op::Add, Flags::NONE, end, m);
                    let keep = s.usize(!mask);
                    Out::Scalar(s.b.binary(Op::And, Flags::NONE, up, keep))
                }
            }
        }
        W::NeedsDrop => {
            let t = t0.ok_or_else(need_t)?;
            let v = u128::from((s.cx.needs_drop)(t));
            Out::Scalar(s.b.iconst(Ty::I8, v))
        }
        W::Forget | W::ColdPath | W::AllocShimMarker => Out::None,
        W::Simd(op) => simd::build(&mut s, c, op)?,
        W::Transmute | W::TransmuteUnchecked => {
            let (from, to) = (t0.ok_or_else(need_t)?, ty_arg(c, 1).ok_or_else(need_t)?);
            let (fid, tid) = (s.layout(from)?, s.layout(to)?);
            let (fl, tl) = (*s.cx.layouts.get(fid), *s.cx.layouts.get(tid));
            if fl.size != tl.size {
                return Err(s.shape("a transmute between types of different sizes (rustc's E0512)"));
            }
            match (c.args.first().copied().ok_or_else(shape)?, tl.scalar) {
                (_, _) if tl.size == 0 => Out::None,
                (Arg::Scalar(v), Some(to)) => {
                    let (fty, _) = lir(fl.scalar.ok_or(s.shape("a scalar argument of a non-scalar type"))?.prim);
                    let (tty, _) = lir(to.prim);
                    Out::Scalar(match (fty == Ty::PTR, tty == Ty::PTR) {
                        _ if fty == tty => v,
                        (true, false) => s.b.convert(Op::Expose, Flags::NONE, tty, v),
                        (false, true) => s.b.convert(Op::FromExposed, Flags::NONE, tty, v),
                        _ => s.b.convert(Op::Bitcast, Flags::NONE, tty, v),
                    })
                }
                (Arg::Place(at), Some(to)) => Out::Scalar(s.b.load(lir(to.prim).0, Flags::NONE, at, 0, MemAttrs::aligned(fl.align))),
                (Arg::Place(at), None) => Out::At(at),
                (Arg::Scalar(v), None) => {
                    let slot = s.b.slot(tl.size as u32, tl.align.max(fl.align), SlotKind::Temp);
                    let at = s.b.slot_addr(slot);
                    s.b.store(Flags::NONE, v, at, 0, MemAttrs::aligned(fl.align));
                    Out::At(at)
                }
                (Arg::None, _) => return Err(shape()),
            }
        }
        W::Copy | W::CopyNonoverlapping | W::WriteBytes | W::VolatileCopyMemory | W::VolatileCopyNonoverlappingMemory | W::VolatileSetMemory => {
            let id = s.layout(t0.ok_or_else(need_t)?)?;
            let l = *s.cx.layouts.get(id);
            let (Some(a), Some(x), Some(count)) = (scalar(0), scalar(1), scalar(2)) else { return Err(shape()) };
            let size = s.usize(l.size);
            let len = s.b.binary(Op::Mul, Flags::NONE, count, size);
            let volatile = matches!(c.which, W::VolatileCopyMemory | W::VolatileCopyNonoverlappingMemory | W::VolatileSetMemory);
            let flags = if volatile { Flags::VOLATILE } else { Flags::NONE };
            let mut attrs = MemAttrs::aligned(l.align);
            match c.which {
                // `copy(src, dst, count)`, `volatile_copy_memory(dst, src, count)`.
                W::Copy => s.b.bulk(Op::Memmove, flags, x, a, len, { attrs.src_align = l.align; attrs }),
                W::CopyNonoverlapping => s.b.bulk(Op::Memcpy, flags, x, a, len, { attrs.src_align = l.align; attrs }),
                W::VolatileCopyMemory => s.b.bulk(Op::Memmove, flags, a, x, len, { attrs.src_align = l.align; attrs }),
                W::VolatileCopyNonoverlappingMemory => s.b.bulk(Op::Memcpy, flags, a, x, len, { attrs.src_align = l.align; attrs }),
                _ => s.b.bulk(Op::Memset, flags, a, x, len, attrs),
            }
            Out::None
        }
        W::ReadViaCopy | W::VolatileLoad | W::UnalignedVolatileLoad => {
            let id = s.layout(t0.ok_or_else(need_t)?)?;
            let l = *s.cx.layouts.get(id);
            let ptr = scalar(0).ok_or_else(shape)?;
            let volatile = c.which != W::ReadViaCopy;
            let align = if c.which == W::UnalignedVolatileLoad { 0 } else { l.align };
            match l.scalar {
                _ if l.is_zst() => Out::None,
                Some(sc) => {
                    let flags = if volatile { Flags::VOLATILE } else { Flags::NONE };
                    Out::Scalar(s.b.load(lir(sc.prim).0, flags, ptr, 0, MemAttrs::aligned(align)))
                }
                None if !volatile => Out::At(ptr),
                None => return Err(IntrinsicError::Unsupported { which: c.which, why: "a volatile load of an aggregate" }),
            }
        }
        W::WriteViaMove | W::VolatileStore | W::UnalignedVolatileStore => {
            let id = s.layout(t0.ok_or_else(need_t)?)?;
            let l = *s.cx.layouts.get(id);
            let ptr = scalar(0).ok_or_else(shape)?;
            let volatile = c.which != W::WriteViaMove;
            let align = if c.which == W::UnalignedVolatileStore { 0 } else { l.align };
            let flags = if volatile { Flags::VOLATILE } else { Flags::NONE };
            match c.args.get(1).copied().ok_or_else(shape)? {
                Arg::None => {}
                Arg::Scalar(v) => s.b.store(flags, v, ptr, 0, MemAttrs::aligned(align)),
                Arg::Place(_) if volatile => {
                    return Err(IntrinsicError::Unsupported { which: c.which, why: "a volatile store of an aggregate" });
                }
                Arg::Place(from) => {
                    let len = s.usize(l.size);
                    let mut attrs = MemAttrs::aligned(align);
                    attrs.src_align = l.align;
                    s.b.bulk(Op::Memcpy, Flags::NONE, ptr, from, len, attrs);
                }
            }
            Out::None
        }
        W::Offset | W::ArithOffset => {
            // Each reads its generics by its own signature: the row says where the element is.
            let element = s.element(c)?;
            let id = s.layout(element)?;
            let size = s.cx.layouts.get(id).size;
            let (Some(ptr), Some(delta)) = (scalar(0), scalar(1)) else { return Err(shape()) };
            let delta = s.resize(delta, Ty::I64, true);
            let sz = s.usize(size);
            let bytes = s.b.binary(Op::Mul, Flags::NONE, delta, sz);
            let flags = if c.which == W::Offset { Flags::INBOUNDS } else { Flags::NONE };
            Out::Scalar(s.b.ptr_add(flags, ptr, bytes))
        }
        W::PtrOffsetFrom | W::PtrOffsetFromUnsigned => {
            let id = s.layout(t0.ok_or_else(need_t)?)?;
            let size = s.cx.layouts.get(id).size;
            if size == 0 {
                return Err(s.shape("`offset_from` of a zero-sized type (core asserts it is not)"));
            }
            let (Some(a), Some(base)) = (scalar(0), scalar(1)) else { return Err(shape()) };
            let a = s.b.convert(Op::PtrAddr, Flags::NONE, Ty::I64, a);
            let base = s.b.convert(Op::PtrAddr, Flags::NONE, Ty::I64, base);
            let diff = s.b.binary(Op::Sub, Flags::NONE, a, base);
            let sz = s.usize(size);
            let op = if c.which == W::PtrOffsetFrom { Op::Sdiv } else { Op::Udiv };
            Out::Scalar(s.b.binary(op, Flags::EXACT, diff, sz))
        }
        W::WrappingAdd | W::WrappingSub | W::WrappingMul | W::SaturatingAdd | W::SaturatingSub | W::UncheckedAdd | W::UncheckedSub | W::UncheckedMul | W::UncheckedDiv | W::UncheckedRem | W::ExactDiv => {
            let (_, signed) = s.scalar(t0.ok_or_else(need_t)?)?;
            let (Some(x), Some(y)) = (scalar(0), scalar(1)) else { return Err(shape()) };
            let promise = if signed { Flags::NSW } else { Flags::NUW };
            let (op, flags) = match c.which {
                W::WrappingAdd => (Op::Add, Flags::NONE),
                W::WrappingSub => (Op::Sub, Flags::NONE),
                W::WrappingMul => (Op::Mul, Flags::NONE),
                W::SaturatingAdd => (if signed { Op::SaddSat } else { Op::UaddSat }, Flags::NONE),
                W::SaturatingSub => (if signed { Op::SsubSat } else { Op::UsubSat }, Flags::NONE),
                W::UncheckedAdd => (Op::Add, promise),
                W::UncheckedSub => (Op::Sub, promise),
                W::UncheckedMul => (Op::Mul, promise),
                W::UncheckedDiv => (if signed { Op::Sdiv } else { Op::Udiv }, Flags::NONE),
                W::UncheckedRem => (if signed { Op::Srem } else { Op::Urem }, Flags::NONE),
                _ => (if signed { Op::Sdiv } else { Op::Udiv }, Flags::EXACT),
            };
            Out::Scalar(s.b.binary(op, flags, x, y))
        }
        W::AddWithOverflow | W::SubWithOverflow | W::MulWithOverflow => {
            let (_, signed) = s.scalar(t0.ok_or_else(need_t)?)?;
            let (Some(x), Some(y)) = (scalar(0), scalar(1)) else { return Err(shape()) };
            let op = match (c.which, signed) {
                (W::AddWithOverflow, false) => Op::UaddOverflow,
                (W::AddWithOverflow, true) => Op::SaddOverflow,
                (W::SubWithOverflow, false) => Op::UsubOverflow,
                (W::SubWithOverflow, true) => Op::SsubOverflow,
                (_, false) => Op::UmulOverflow,
                (_, true) => Op::SmulOverflow,
            };
            let pair = s.b.overflow(op, x, y);
            let v = s.b.project(pair, 0);
            let o = s.b.project(pair, 1);
            let o = s.b.convert(Op::Zext, Flags::NONE, Ty::I8, o);
            Out::Pair(v, o)
        }
        W::UncheckedShl | W::UncheckedShr | W::RotateLeft | W::RotateRight => {
            let (ty, signed) = s.scalar(t0.ok_or_else(need_t)?)?;
            let (Some(x), Some(y)) = (scalar(0), scalar(1)) else { return Err(shape()) };
            let y = s.resize(y, ty, false);
            let (op, flags) = match c.which {
                W::UncheckedShl => (Op::Shl, Flags::IN_RANGE),
                W::UncheckedShr => (if signed { Op::Ashr } else { Op::Lshr }, Flags::IN_RANGE),
                W::RotateLeft => (Op::Rotl, Flags::NONE),
                _ => (Op::Rotr, Flags::NONE),
            };
            Out::Scalar(s.b.binary(op, flags, x, y))
        }
        W::Ctpop | W::Ctlz | W::CtlzNonzero | W::Cttz | W::CttzNonzero => {
            let x = scalar(0).ok_or_else(shape)?;
            let (op, flags) = match c.which {
                W::Ctpop => (Op::Popcnt, Flags::NONE),
                W::Ctlz => (Op::Clz, Flags::NONE),
                W::CtlzNonzero => (Op::Clz, Flags::NONZERO),
                W::Cttz => (Op::Ctz, Flags::NONE),
                _ => (Op::Ctz, Flags::NONZERO),
            };
            let n = s.b.unary(op, flags, x);
            Out::Scalar(s.resize(n, Ty::I32, false))
        }
        W::Bswap | W::Bitreverse => {
            let x = scalar(0).ok_or_else(shape)?;
            if c.which == W::Bswap && s.b.ty(x) == Ty::I8 {
                Out::Scalar(x)
            } else {
                Out::Scalar(s.b.unary(if c.which == W::Bswap { Op::Bswap } else { Op::Bitrev }, Flags::NONE, x))
            }
        }
        W::Likely | W::Unlikely | W::BlackBox => match c.args.first().copied().ok_or_else(shape)? {
            Arg::Scalar(v) => Out::Scalar(v),
            Arg::Place(at) => Out::At(at),
            Arg::None => Out::None,
        },
        W::Assume => {
            let x = scalar(0).ok_or_else(shape)?;
            let zero = s.b.iconst(Ty::I8, 0);
            let cond = s.b.icmp(IntCond::Ne, x, zero);
            s.b.assume(cond);
            Out::None
        }
        W::Unreachable => {
            s.b.unreachable();
            Out::Diverges
        }
        W::AbortImmediate => {
            s.b.trap(ABORT_TRAP);
            Out::Diverges
        }
        W::DiscriminantValue => discriminant(&mut s, c, t0.ok_or_else(need_t)?)?,
        W::AtomicLoad | W::AtomicStore => {
            let (ty, _) = s.scalar(t0.ok_or_else(need_t)?)?;
            let id = s.layout(t0.ok_or_else(need_t)?)?;
            let attrs = MemAttrs::aligned(s.cx.layouts.get(id).align);
            let ord = const_arg(c, 1).and_then(ordering).ok_or(s.shape("an ordering that is not an `AtomicOrdering`"))?;
            let volatile = const_arg(c, 2).ok_or(s.shape("a missing VOLATILE argument"))? != 0;
            let flags = Flags::NONE.with_ordering(ord);
            let flags = if volatile { flags.union(Flags::VOLATILE) } else { flags };
            let ptr = scalar(0).ok_or_else(shape)?;
            if c.which == W::AtomicLoad {
                if matches!(ord, Ordering::Release | Ordering::AcqRel) {
                    return Err(s.shape("an atomic load with a release ordering"));
                }
                Out::Scalar(s.b.atomic_load(ty, flags, ptr, attrs))
            } else {
                if matches!(ord, Ordering::Acquire | Ordering::AcqRel) {
                    return Err(s.shape("an atomic store with an acquire ordering"));
                }
                s.b.atomic_store(flags, scalar(1).ok_or_else(shape)?, ptr, attrs);
                Out::None
            }
        }
        W::AtomicXchg | W::AtomicXadd | W::AtomicXsub | W::AtomicAnd | W::AtomicNand | W::AtomicOr | W::AtomicXor | W::AtomicMax | W::AtomicMin | W::AtomicUmax | W::AtomicUmin => {
            let t = t0.ok_or_else(need_t)?;
            let (ty, _) = s.scalar(t)?;
            let id = s.layout(t)?;
            let attrs = MemAttrs::aligned(s.cx.layouts.get(id).align);
            let ord = c.generics.last().and_then(|g| match g {
                GenericArg::Const(v) => ordering(*v),
                GenericArg::Ty(_) => None,
            });
            let ord = ord.ok_or(s.shape("an ordering that is not an `AtomicOrdering`"))?;
            let (Some(ptr), Some(v)) = (scalar(0), scalar(1)) else { return Err(shape()) };
            let rmw = match c.which {
                W::AtomicXchg => AtomicRmw::Xchg,
                W::AtomicXadd => AtomicRmw::Add,
                W::AtomicXsub => AtomicRmw::Sub,
                W::AtomicAnd => AtomicRmw::And,
                W::AtomicNand => AtomicRmw::Nand,
                W::AtomicOr => AtomicRmw::Or,
                W::AtomicXor => AtomicRmw::Xor,
                W::AtomicMax => AtomicRmw::Max,
                W::AtomicMin => AtomicRmw::Min,
                W::AtomicUmax => AtomicRmw::Umax,
                _ => AtomicRmw::Umin,
            };
            let v = if s.b.ty(v) == Ty::PTR { s.b.convert(Op::Expose, Flags::NONE, Ty::I64, v) } else { v };
            let old = s.b.atomic_rmw(rmw, Flags::NONE.with_ordering(ord), ptr, v, attrs);
            Out::Scalar(if ty == Ty::PTR { s.b.convert(Op::FromExposed, Flags::NONE, Ty::PTR, old) } else { old })
        }
        W::AtomicCxchg | W::AtomicCxchgWeak => {
            let t = t0.ok_or_else(need_t)?;
            let (ty, _) = s.scalar(t)?;
            let id = s.layout(t)?;
            let attrs = MemAttrs::aligned(s.cx.layouts.get(id).align);
            let ok = const_arg(c, 1).and_then(ordering);
            let fail = const_arg(c, 2).and_then(ordering);
            let (Some(ok), Some(fail)) = (ok, fail) else { return Err(s.shape("orderings that are not `AtomicOrdering`s")) };
            if matches!(fail, Ordering::Release | Ordering::AcqRel) {
                return Err(s.shape("a compare-exchange failure ordering with release"));
            }
            let (Some(ptr), Some(old), Some(new)) = (scalar(0), scalar(1), scalar(2)) else { return Err(shape()) };
            let mut flags = Flags::NONE.with_ordering(ok).with_failure(fail);
            if c.which == W::AtomicCxchgWeak {
                flags = flags.union(Flags::WEAK);
            }
            let (old, new) = if ty == Ty::PTR {
                (s.b.convert(Op::Expose, Flags::NONE, Ty::I64, old), s.b.convert(Op::Expose, Flags::NONE, Ty::I64, new))
            } else {
                (old, new)
            };
            let pair = s.b.cmpxchg(flags, ptr, old, new, attrs);
            let v = s.b.project(pair, 0);
            let v = if ty == Ty::PTR { s.b.convert(Op::FromExposed, Flags::NONE, Ty::PTR, v) } else { v };
            let done = s.b.project(pair, 1);
            let done = s.b.convert(Op::Zext, Flags::NONE, Ty::I8, done);
            Out::Pair(v, done)
        }
        W::AtomicFence | W::AtomicSingleThreadFence => {
            let ord = const_arg(c, 0).and_then(ordering).ok_or(s.shape("an ordering that is not an `AtomicOrdering`"))?;
            if ord == Ordering::Relaxed {
                return Err(s.shape("a relaxed fence (core panics on one)"));
            }
            let flags = Flags::NONE.with_ordering(ord);
            s.b.fence(if c.which == W::AtomicSingleThreadFence { flags.union(Flags::SINGLE_THREAD) } else { flags });
            Out::None
        }
        W::CallerLocation => {
            let Some((site, loc_ty)) = c.site else { return Err(s.shape("`caller_location` without its site") ) };
            match site {
                Site::Passed(v) => Out::Scalar(v),
                Site::Here { file, line, col } => Out::Scalar(location(&mut s, loc_ty, file, line, col)?),
            }
        }
        // `ptr_metadata::<P, M>(p)`: a wide pointer's second word (a slice's or str's length,
        // lib.rs "Wide pointers"), read as `M`; a thin pointer's is `()`.
        W::PtrMetadata => match c.args.first().copied().ok_or_else(shape)? {
            Arg::Scalar(_) | Arg::None => Out::None,
            Arg::Place(at) => {
                let (ty, _) = s.scalar(c.ret)?;
                Out::Scalar(s.b.load(ty, Flags::NONE, at, 8, MemAttrs::aligned(3)))
            }
        },
        // `aggregate_raw_ptr::<P, D, M>(data, meta)`: the data pointer when the metadata is
        // `()`, else the two words of the wide pointer.
        W::AggregateRawPtr => {
            let data = scalar(0).ok_or_else(shape)?;
            match c.args.get(1).copied().ok_or_else(shape)? {
                Arg::None => Out::Scalar(data),
                Arg::Scalar(meta) => Out::Pair(data, meta),
                Arg::Place(_) => return Err(s.shape("pointer metadata that is not one word")),
            }
        }
        // `ptr_guaranteed_cmp(a, b)`: at run time the addresses are known, so the answer is
        // never 2 ("unknown"): 1 when equal, else 0.
        W::PtrGuaranteedCmp => {
            let (Some(a), Some(b)) = (scalar(0), scalar(1)) else {
                return Err(IntrinsicError::Unsupported { which: c.which, why: "it on wide pointers" });
            };
            let eq = s.b.icmp(IntCond::Eq, a, b);
            Out::Scalar(s.b.convert(Op::Zext, Flags::NONE, Ty::I8, eq))
        }
        // `three_way_compare::<T>(lhs, rhs)`: "Does a three-way comparison between the two
        // arguments, which must be of character or integer (signed or unsigned) type." The
        // result is `cmp::Ordering` (`#[repr(i8)]`: Less = -1, Equal = 0, Greater = 1), its
        // scalar the call's type; `char` compares as its unsigned code point.
        W::ThreeWayCompare => {
            let t = t0.ok_or_else(need_t)?;
            let signed = match s.node(t) {
                Some(PNode::Int(_)) => true,
                Some(PNode::Uint(_) | PNode::Char) => false,
                // The declaration's own rule ("which must be of character or integer type").
                _ => return Err(s.shape("`three_way_compare` of a type that is not an integer or `char`")),
            };
            let (Some(x), Some(y)) = (scalar(0), scalar(1)) else { return Err(shape()) };
            let (rty, _) = s.scalar(c.ret)?;
            let (lt, gt) = if signed { (IntCond::Slt, IntCond::Sgt) } else { (IntCond::Ult, IntCond::Ugt) };
            let less = s.b.icmp(lt, x, y);
            let greater = s.b.icmp(gt, x, y);
            // `iconst` truncates to the width: all ones is -1.
            let minus_one = s.b.iconst(rty, u128::MAX);
            let zero = s.b.iconst(rty, 0);
            let one = s.b.iconst(rty, 1);
            let not_less = s.b.select(Flags::NONE, greater, one, zero);
            Out::Scalar(s.b.select(Flags::NONE, less, minus_one, not_less))
        }
        // `slice_get_unchecked::<ItemPtr, SlicePtr, T>(slice_ptr, index)`: "Projects to the
        // `index`-th element of `slice_ptr` ... without a bounds check" (`index <
        // PtrMetadata(slice_ptr)` is the caller's promise, so nothing is checked). The wide
        // pointer is an aggregate (its data word at 0, its length at 8, as `ptr_metadata` reads
        // it); the result is the thin pointer of the same kind.
        W::SliceGetUnchecked => {
            let slice_ptr = ty_arg(c, 1).ok_or_else(need_t)?;
            let pointee = s.pointee(slice_ptr)?;
            // `ItemPtr: ChangePointee<[T], Pointee = T, Output = SlicePtr>` makes it a pointer to `[T]`.
            let Some(PNode::Slice(elem)) = s.node(pointee) else {
                return Err(s.shape("`slice_get_unchecked` of a `SlicePtr` that is not a pointer to a slice"));
            };
            if ty_arg(c, 2).is_some_and(|t| t != elem) {
                return Err(s.shape("`slice_get_unchecked`'s `T` is not the element of its `SlicePtr`"));
            }
            let (Some(Arg::Place(at)), Some(index)) = (c.args.first().copied(), scalar(1)) else { return Err(shape()) };
            let data = s.b.load(Ty::PTR, Flags::NONE, at, 0, MemAttrs::aligned(3));
            let id = s.layout(elem)?;
            let size = s.cx.layouts.get(id).size;
            if size == 0 {
                Out::Scalar(data)
            } else {
                let index = s.resize(index, Ty::I64, false);
                let sz = s.usize(size);
                let bytes = s.b.binary(Op::Mul, Flags::NUW, index, sz);
                Out::Scalar(s.b.ptr_add(Flags::INBOUNDS, data, bytes))
            }
        }
        // A call of its run-time fn, which only the walk can make (lib.rs, "Intrinsics").
        W::ConstEvalSelect => return Err(s.shape("`const_eval_select` reached the intrinsic builder: the walk calls its run-time fn")),
        W::CatchUnwind => return Err(s.shape("`catch_unwind` reached the intrinsic builder: the walk builds its landing block")),
        // core's `ub_checks` has a body (`cfg!(ub_checks)`), which front2 builds as the fn
        // (front2/program/tabled.rs: an intrinsic is tabled only without a body). A table call
        // would need the build's own `ub_checks` setting, which no builder context carries yet.
        W::UbChecks => return Err(IntrinsicError::Unsupported { which: c.which, why: "`ub_checks` as a table call (front2 builds its `cfg!(ub_checks)` body)" }),
    };
    Ok(out)
}

/// `discriminant_value::<T>(v)`: the tag at `v` decoded by `T`'s encoding, as the call's type.
fn discriminant<LA: Allocator + Clone, S: Sink, A: Allocator + Clone>(
    s: &mut B<'_, '_, '_, '_, LA, S, A>,
    c: &Call<'_>,
    t: PTy,
) -> Result<Out, IntrinsicError> {
    let id = s.layout(t)?;
    let (rty, _) = s.scalar(c.ret)?;
    let at = match s.cx.layouts.get(id).variants {
        Variants::Single { .. } => None,
        Variants::Multiple { .. } => match c.args.first().copied() {
            Some(Arg::Scalar(ptr)) => Some((ptr, 0)),
            _ => return Err(s.shape("the arguments are not of the intrinsic's signature")),
        },
    };
    let d = read_discriminant(s.b, &*s.cx.layouts, id, at, rty).ok_or(s.shape("an enum whose tag is past 2 GiB"))?;
    Ok(Out::Scalar(d))
}

/// `v` converted to `to`'s width, zero- or sign-extended.
fn resize_in<LA: Allocator + Clone, S: Sink>(b: &mut Builder<LA, S>, v: Value, to: Ty, signed: bool) -> Value {
    let (fb, tb) = (b.ty(v).bits(64), to.bits(64));
    if fb == tb {
        v
    } else if fb > tb {
        b.convert(Op::Trunc, Flags::NONE, to, v)
    } else {
        b.convert(if signed { Op::Sext } else { Op::Zext }, Flags::NONE, to, v)
    }
}

/// The discriminant of the value of layout `id` at `at` (a pointer and a byte offset), as an
/// integer of LIR type `rty`: its tag read and decoded by the layout's encoding, or a constant
/// for a layout without a tag (`at` is then not read and may be `None`). The one decoder of a
/// tag: `discriminant_value` and the walk's `match` and enum copy all read it here (rule 30).
/// `None` when the layout has a tag and `at` is `None`, or the tag's offset overflows `i32`.
pub(crate) fn read_discriminant<LA: Allocator + Clone, S: Sink, A: Allocator + Clone>(
    b: &mut Builder<LA, S>,
    layouts: &Layouts<A>,
    id: LayoutId,
    at: Option<(Value, i32)>,
    rty: Ty,
) -> Option<Value> {
    let l = *layouts.get(id);
    let constant = |b: &mut Builder<LA, S>, d: i128| b.iconst(rty, d as u128 & crate::layout::mask(rty.bits(64)));
    let Variants::Multiple { tag, tag_offset, encoding, count, .. } = l.variants else {
        // A struct has no discriminants: 0, as `discriminant_value` of a non-enum.
        let d = match l.variants {
            Variants::Single { index } => layouts.discriminant(id, index).unwrap_or(0),
            Variants::Multiple { .. } => 0,
        };
        return Some(constant(b, d));
    };
    let (ptr, offset) = at?;
    let offset = offset.checked_add(i32::try_from(tag_offset).ok()?)?;
    // A pointer niche is read as its address bits.
    let (tty, tsigned) = match lir(tag.prim) {
        (Ty::PTR, _) => (Ty::I64, false),
        other => other,
    };
    let bits = b.load(tty, Flags::NONE, ptr, offset, MemAttrs::aligned(tag.prim.align()));
    let identity = (0..count).all(|v| layouts.discriminant(id, v) == Some(i128::from(v)));
    let variant = match encoding {
        // The tag is the discriminant at the tag's width: extended by the tag's sign.
        TagEncoding::Direct => return Some(resize_in(b, bits, rty, tsigned)),
        TagEncoding::Niche { untagged, first, last, start } => {
            let start = b.iconst(tty, start);
            let rel = b.binary(Op::Sub, Flags::NONE, bits, start);
            let span = b.iconst(tty, u128::from(last - first));
            let in_niche = b.icmp(IntCond::Ule, rel, span);
            let rel = resize_in(b, rel, rty, false);
            let first = b.iconst(rty, u128::from(first));
            let niched = b.binary(Op::Add, Flags::NONE, rel, first);
            let untagged = b.iconst(rty, u128::from(untagged));
            b.select(Flags::NONE, in_niche, niched, untagged)
        }
    };
    if identity {
        return Some(variant);
    }
    // Each variant index to its discriminant, a select per variant.
    let mut out = constant(b, layouts.discriminant(id, 0).unwrap_or(0));
    for v in 1..count {
        let d = layouts.discriminant(id, v).unwrap_or(0);
        let k = b.iconst(rty, u128::from(v));
        let is = b.icmp(IntCond::Eq, variant, k);
        let d = constant(b, d);
        out = b.select(Flags::NONE, is, d, out);
    }
    Some(out)
}

/// A `Location { filename, line, col }` as a data row of the function; its address.
fn location<LA: Allocator + Clone, S: Sink, A: Allocator + Clone>(
    s: &mut B<'_, '_, '_, '_, LA, S, A>,
    loc_ty: PTy,
    file: &str,
    line: u32,
    col: u32,
) -> Result<Value, IntrinsicError> {
    let id = s.layout(loc_ty)?;
    let l = *s.cx.layouts.get(id);
    let offset = |s: &B<'_, '_, '_, '_, LA, S, A>, i: u32| {
        s.cx.layouts.field_offset(id, i).ok_or(s.shape("`Location` without its fields (filename, line, col)"))
    };
    let (f, ln, cl) = (offset(s, 0)?, offset(s, 1)?, offset(s, 2)?);
    if f + 16 > l.size || ln + 4 > l.size || cl + 4 > l.size {
        return Err(s.shape("`Location`'s fields are not a wide pointer and two `u32`s"));
    }
    // The file's bytes and a NUL after them (core's `Location::file_as_c_str`).
    let mut name = [0u8; 256];
    let len = file.len();
    if len >= name.len() {
        return Err(IntrinsicError::Unsupported { which: s.which, why: "a `Location` of a path of 256 bytes or more" });
    }
    name[..len].copy_from_slice(file.as_bytes());
    let text = s.b.data(&name[..len + 1], 0, &[]);
    let mut bytes = [0u8; 64];
    if l.size > bytes.len() as u64 {
        return Err(s.shape("a `Location` past 64 bytes"));
    }
    let (f, ln, cl) = (f as usize, ln as usize, cl as usize);
    bytes[f + 8..f + 16].copy_from_slice(&(len as u64).to_le_bytes());
    bytes[ln..ln + 4].copy_from_slice(&line.to_le_bytes());
    bytes[cl..cl + 4].copy_from_slice(&col.to_le_bytes());
    let row = s.b.data(&bytes[..l.size as usize], u32::from(l.align), &[DataReloc { offset: f as u32, target: text, addend: 0 }]);
    Ok(s.b.data_addr(row))
}

mod element;
pub mod simd;
pub mod target;
#[cfg(test)]
mod tests;

pub use element::ElementArg;
