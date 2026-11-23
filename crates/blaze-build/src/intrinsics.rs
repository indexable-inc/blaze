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

use blaze_lir::builder::MemAttrs;
use blaze_lir::func::SlotKind;
use blaze_lir::{AtomicRmw, Builder, Flags, IntCond, Op, Ordering, Sink, Ty, Value};
use blaze_typeck::{AdtKind, ItemKey, PNode, PTy, PTys};

use crate::layout::{LayoutCx, LayoutError, LayoutFacts, LayoutId, Layouts, Prim, Tail, TagEncoding, Variants};

/// The trap code of `abort_immediate`: `brk #0xfe00` on AArch64 (one home: blaze-code trap.rs).
pub const ABORT_TRAP: u32 = blaze_code::trap::ABORT;

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
    /// alloc's `write_box_via_move::<T>(b, x)`: `x` written into the box's memory, the box
    /// returned.
    WriteBoxViaMove,
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
    /// The validity assertions: decided at the instance by the layout (intrinsics/validity.rs),
    /// built by the walk (walk/validity.rs) as nothing or a `panic_nounwind` call.
    AssertInhabited,
    AssertZeroValid,
    AssertMemUninitializedValid,
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
    /// `type_name::<T>()`: evaluated by blaze-consteval only (core calls it in a `const`
    /// block); its text is type_name.rs's.
    TypeName,
    /// `type_id::<T>()` (core's `intrinsics::reflection`, called in `TypeId::of`'s `const`
    /// block): the 128 bits of [`crate::instance::type_id_hash`], laid out as `TypeId`'s two
    /// `data` words; the constant evaluator and the builder both make it from that function.
    TypeId,
    /// `float_to_int_unchecked::<F, I>(value)`: `fptosi`/`fptoui` by `I`'s signedness.
    FloatToIntUnchecked,
    /// core's `ub_checks()`: `true` in a constant (blaze-consteval machine.rs `intrinsic`); at
    /// run time the building profile's answer ([`IntrinsicCx::ub_checks`]), never its body's
    /// `cfg!(ub_checks)`, which is the library crate's.
    UbChecks,
    /// core's `overflow_checks()`: `true` in a constant (blaze-consteval machine.rs
    /// `intrinsic`), as `ub_checks`; at run time the building profile's overflow setting
    /// ([`IntrinsicCx::overflow_checks`]).
    OverflowChecks,
    /// `ptr_mask::<T>(ptr, mask)`: the address `and` the mask, the provenance kept.
    PtrMask,
    /// `raw_eq::<T>(a, b)`: whether the `size_of::<T>()` bytes behind both are equal
    /// ([`byte_order`] the rule the constant evaluator computes).
    RawEq,
    /// `compare_bytes(left, right, bytes)`: [`byte_order`] of the two ranges.
    CompareBytes,
    /// blaze's `blaze_field_count::<T>() -> usize`: compile-time field count of a structural
    /// record, tuple, or struct (`0` for enums, unions, and other types; [`field_count`] the rule).
    BlazeFieldCount,
    /// blaze's `blaze_int_cast::<S, D>(x)`: `x as D` ([`int_cast_bits`] the rule).
    BlazeIntCast,
    /// blaze's `blaze_record_schema_id::<T>() -> u64`: canonical structural schema hash of a
    /// record (`0` for non-records; [`record_schema_id`] the rule).
    BlazeRecordSchemaId,
    /// blaze's `blaze_records_disjoint::<A, B>() -> bool`: whether two structural record types
    /// share zero field names ([`records_disjoint`] the rule).
    BlazeRecordsDisjoint,
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
    e("align_of_val", Which::AlignOfVal, "The required alignment of the referenced value.", "a constant: a slice tail's alignment is static; of `dyn` or a struct ending in it, the prefix's alignment and the vtable's word 2, the larger"),
    e("arith_offset", Which::ArithOffset, "Calculates the offset from a pointer, potentially wrapping.", "`ptr_add` of `delta * size`, no promise").stepping(ElementArg::Is(0)),
    e("assert_inhabited", Which::AssertInhabited, "A guard for unsafe functions that cannot ever be executed if `T` is uninhabited: This will statically either panic, or do nothing.", "nothing, or (T uninhabited: an enum with no variant whose fields are all inhabited) a call of `panic_nounwind` \"attempted to instantiate uninhabited type `T`\" (intrinsics/validity/exact.rs, walk/validity.rs)"),
    e("assert_mem_uninitialized_valid", Which::AssertMemUninitializedValid, "A guard for `std::mem::uninitialized`. This will statically either panic, or do nothing.", "nothing, or (the value of T whose every byte is 0x01 invalid, an enum by the variant that fill selects: intrinsics/validity/exact.rs) a call of `panic_nounwind` \"attempted to leave type `T` uninitialized, which is invalid\""),
    e("assert_zero_valid", Which::AssertZeroValid, "A guard for unsafe functions that cannot ever be executed if `T` does not permit zero-initialization: This will statically either panic, or do nothing.", "nothing, or (the all-zero value of T invalid, an enum by the variant whose tag reads 0: intrinsics/validity/exact.rs) a call of `panic_nounwind` \"attempted to zero-initialize type `T`, which is invalid\""),
    e("assume", Which::Assume,"Informs the optimizer that a condition is always true. If the condition is false, the behavior is undefined.", "`assume`"),
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
    e("blaze_field_count", Which::BlazeFieldCount, "blaze: compile-time field count of a structural record, tuple, or struct (`0` for enums, unions, and other types).", "a constant `usize` from `field_count`"),
    e("blaze_int_cast", Which::BlazeIntCast, "blaze: `x as D` between two integer types of any width, the standard ones and those of `#![blaze::arbitrary_ints]` (`u3`, `i24`): it truncates to `D`'s width, or extends by `S`'s sign, and never panics.", "`trunc`, or `sext`/`zext` by `S`'s sign, or the value itself at one width (`int_cast_bits` the rule)"),
    e("blaze_record_schema_id", Which::BlazeRecordSchemaId, "blaze: canonical compile-time 64-bit structural schema ID of a record (`0` for non-records).", "a constant `u64` from `record_schema_id`"),
    e("blaze_records_disjoint", Which::BlazeRecordsDisjoint, "blaze: compile-time check that two structural record types share zero field names.", "a constant `bool` from `records_disjoint`"),
    e("bswap", Which::Bswap, "Reverses the bytes in an integer type `T`.", "`bswap` (a byte: itself)"),
    e("caller_location", Which::CallerLocation, "Gets a reference to a static `Location` indicating where it was called.", "the address of a data row of the site's `Location`, or the location a `#[track_caller]` fn was passed"),
    e("catch_unwind", Which::CatchUnwind, "Rust's \"try catch\" construct for unwinding.", "an `invoke` of `try_fn` whose `catch` landing block calls `catch_fn` (walk/unwind.rs); under `panic = abort` a call and `false`"),
    e("cold_path", Which::ColdPath, "Hints to the compiler that current code path is cold.", "nothing"),
    e("compare_bytes", Which::CompareBytes, "Lexicographically compare `[left, left + bytes)` and `[right, right + bytes)` as unsigned bytes, returning negative if `left` is less, zero if all the bytes match, or positive if `left` is greater.", "one `memcmp` (`byte_order` the rule)"),
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
    e("float_to_int_unchecked", Which::FloatToIntUnchecked, "Converts with LLVM’s fptoui/fptosi, which may return undef for values out of range (<https://github.com/rust-lang/rust/issues/10184>). Stabilized as [`f32::to_int_unchecked`] and [`f64::to_int_unchecked`]: the value must not be `NaN` or infinite, and must be representable in the return type once truncated, else the behavior is undefined.", "`fptosi`/`fptoui` by the integer's signedness (saturating: a defined value where Rust's is undefined)"),
    e("forget", Which::Forget, "Moves a value out of scope without running drop glue.", "nothing"),
    e("likely", Which::Likely, "Hints to the compiler that branch condition is likely to be true. Returns the value passed to it.", "its argument"),
    e("mul_with_overflow", Which::MulWithOverflow, "Performs checked integer multiplication", "`umul_overflow`/`smul_overflow`"),
    e("needs_drop", Which::NeedsDrop, "Returns `true` if the actual type given as `T` requires drop glue; returns `false` if the actual type provided for `T` implements `Copy`.", "a constant from the drop facts"),
    e("offset", Which::Offset, "Calculates the offset from a pointer.", "`ptr_add inbounds` of `delta * size`").stepping(ElementArg::PointeeOf(0)),
    e("overflow_checks", Which::OverflowChecks, "Returns whether we should perform some overflow-checking at runtime.", "the building profile's `overflow-checks`, a constant; `true` in a constant"),
    e("ptr_guaranteed_cmp", Which::PtrGuaranteedCmp, "See documentation of `<*const T>::guaranteed_eq` for details.", "`icmp eq` zero-extended to `u8` (never 2 at run time); wide pointers refused by name; the constant evaluator answers 2 (unknown) unless certain"),
    e("ptr_mask", Which::PtrMask, "Masks out bits of the pointer according to a mask.", "`ptr_addr`, `and` the mask, `ptr_with_addr` (the provenance kept)"),
    e("ptr_metadata", Which::PtrMetadata, "Lowers in MIR to `Rvalue::UnaryOp` with `UnOp::PtrMetadata`.", "a wide pointer's word at 8, read as `M`; `()` for a thin pointer"),
    e("ptr_offset_from", Which::PtrOffsetFrom, "See documentation of `<*const T>::offset_from` for details.", "the address difference `sdiv exact` the size; of a zero-sized `T` (\"This function panics if `T` is a Zero-Sized Type\", checked before the call) `trap` ABORT_TRAP"),
    e("ptr_offset_from_unsigned", Which::PtrOffsetFromUnsigned, "See documentation of `<*const T>::offset_from_unsigned` for details.", "the address difference `udiv exact` the size; of a zero-sized `T` (core asserts `0 < pointee_size` before the call) `trap` ABORT_TRAP"),
    e("raw_eq", Which::RawEq, "Determines whether the raw bytes of the two values are equal.", "nothing for a zero-sized `T`; up to 16 pairs of loads `xor`ed and `or`ed, `== 0`; past that `memcmp == 0` (`byte_order` the rule)"),
    e("read_via_copy", Which::ReadViaCopy, "This is an implementation detail of [`crate::ptr::read`] and should not be used anywhere else.", "a `load` of a scalar; an aggregate is the `T` at the pointer"),
    e("rotate_left", Which::RotateLeft, "Performs rotate left.", "`rotl`, the amount at `T`'s width"),
    e("rotate_right", Which::RotateRight, "Performs rotate right.", "`rotr`, the amount at `T`'s width"),
    e("saturating_add", Which::SaturatingAdd, "Computes `a + b`, saturating at numeric bounds.", "`uadd_sat`/`sadd_sat`"),
    e("saturating_sub", Which::SaturatingSub, "Computes `a - b`, saturating at numeric bounds.", "`usub_sat`/`ssub_sat`"),
    e("size_of", Which::SizeOf, "The size of a type in bytes.", "a constant from the layout"),
    e("size_of_val", Which::SizeOfVal, "The size of the referenced value in bytes.", "a constant, or the slice length times the stride plus the tail's offset (the prefix unrounded), rounded once; of `dyn`, the vtable's word 1; of a struct ending in `dyn`, the prefix rounded up to the vtable's word 2, plus word 1, rounded up to the larger alignment"),
    e("slice_get_unchecked", Which::SliceGetUnchecked, "Projects to the `index`-th element of `slice_ptr`, as the same kind of pointer as the slice was provided -- so `&mut [T] → &mut T`, `&[T] → &T`, `*mut [T] → *mut T`, or `*const [T] → *const T` -- without a bounds check.", "the data word, `ptr_add inbounds` of `index * size_of::<T>()`; a zero-sized `T`: the data word"),
    e("sub_with_overflow", Which::SubWithOverflow, "Performs checked integer subtraction", "`usub_overflow`/`ssub_overflow`"),
    e("three_way_compare", Which::ThreeWayCompare, "Does a three-way comparison between the two arguments, which must be of character or integer (signed or unsigned) type.", "two `icmp`s (`slt`/`sgt`, or `ult`/`ugt` for unsigned and `char`) and two `select`s to `Ordering`'s -1/0/1"),
    e("transmute", Which::Transmute, "Reinterprets the bits of a value of one type as another type.", "the bits kept (`expose`/`from_exposed`/`bitcast` between scalar kinds); an aggregate through memory"),
    e("transmute_unchecked", Which::TransmuteUnchecked, "Like [`transmute`], but even less checked at compile-time: rather than giving an error for `size_of::<Src>() != size_of::<Dst>()`, it's **Undefined Behavior** at runtime.", "as `transmute`; sizes must still agree here"),
    e("type_id", Which::TypeId, "Gets an identifier which is globally unique to the specified type. This function will return the same value for a type regardless of whichever crate it is invoked in.", "the 128 bits of `instance::type_id_hash` (the type's ground tree, lifetimes erased) as `TypeId`'s two words, little-endian; blaze-consteval makes the same bytes from the same function"),
    e("type_name", Which::TypeName, "Gets a static string slice containing the name of a type.", "nothing at run time (\"can only be called at compile-time\"): blaze-consteval evaluates it to the text of type_name.rs"),
    e("ub_checks", Which::UbChecks, "Returns whether we should perform some UB-checking at runtime.", "the building profile's `debug-assertions`, a constant; `true` in a constant"),
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
    e("volatile_load", Which::VolatileLoad, "Performs a volatile load from the `src` pointer.", "`load volatile`; an aggregate as `load volatile`s of its alignment's width (at most 8 bytes, a pointer's word as a pointer), at most 512, into a temporary"),
    e("volatile_set_memory", Which::VolatileSetMemory, "Equivalent to the appropriate `llvm.memset.p0i8.*` intrinsic, with a size of `count * size_of::<T>()` and an alignment of `align_of::<T>()`.", "`memset volatile`"),
    e("volatile_store", Which::VolatileStore, "Performs a volatile store to the `dst` pointer.", "`store volatile`; an aggregate as `store volatile`s of its alignment's width (at most 8 bytes), at most 512"),
    e("wrapping_add", Which::WrappingAdd, "Wrapping (modular) addition. Computes `a + b`, wrapping around at the boundary of the type.", "`add`"),
    e("wrapping_mul", Which::WrappingMul, "Wrapping (modular) multiplication. Computes `a * b`, wrapping around at the boundary of the type.", "`mul`"),
    e("wrapping_sub", Which::WrappingSub, "Wrapping (modular) subtraction. Computes `a - b`, wrapping around at the boundary of the type.", "`sub`"),
    e("write_box_via_move", Which::WriteBoxViaMove, "Writes `x` into `b`.", "`x` stored (a scalar) or `memcpy`d (an aggregate) at the box's pointer, which is the result"),
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
    /// The intrinsic's own call site, or a tracked call's, item-relative (rule 71).
    Here(crate::CallerSite<'s>),
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
    /// What `ub_checks()` is in the code built ([`crate::Adts::ub_checks`]).
    pub ub_checks: crate::UbChecks,
    /// What `overflow_checks()` is in the code built ([`crate::Adts::overflow_checks`]).
    pub overflow_checks: crate::OverflowChecks,
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
pub fn lir(prim: Prim) -> (Ty, bool) {
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
                // size at word 1 and the alignment at word 2; a struct ending in a trait object
                // adds its sized prefix (`dyn_tail::size_and_align`; a bare `dyn` is the empty
                // prefix, so its answer is the vtable's words themselves).
                (Tail::Dyn, _) => {
                    let vt = match c.args.first() {
                        Some(Arg::Place(at)) => s.b.load(Ty::PTR, Flags::NONE, *at, 8, MemAttrs::aligned(3)),
                        _ => return Err(shape()),
                    };
                    let prefix = s.cx.layouts.dyn_chain(id).ok_or(s.shape("a `dyn` tail whose row has no prefix"))?;
                    let (size, align) = crate::dyn_tail::size_and_align(&mut *s.b, prefix, vt);
                    Out::Scalar(if c.which == W::SizeOfVal { size } else { align })
                }
                (_, W::AlignOfVal) => Out::Scalar(s.usize(l.align_bytes())),
                (Tail::Sized, _) => Out::Scalar(s.usize(l.size)),
                (Tail::Slice { stride }, _) => {
                    // The length is the wide pointer's metadata, at 8.
                    let len = match c.args.first() {
                        Some(Arg::Place(at)) => s.b.load(Ty::I64, Flags::NONE, *at, 8, MemAttrs::aligned(3)),
                        _ => return Err(shape()),
                    };
                    // `l.size` of an unsized row is its tail's offset, unrounded (layout.rs
                    // `univariant`).
                    Out::Scalar(crate::dyn_tail::slice_size(&mut *s.b, l.size, stride, l.align, len))
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
                // An aggregate: `load volatile`s of the pieces of its bytes, each stored into a
                // temporary the walk copies out.
                None => {
                    let p = Pieces::of(l.size, align).ok_or(IntrinsicError::Unsupported { which: c.which, why: "a volatile load of an aggregate past 512 accesses" })?;
                    let slot = s.b.slot(l.size as u32, l.align, SlotKind::Temp);
                    let at = s.b.slot_addr(slot);
                    for off in p.offsets() {
                        let v = s.b.load(p.ty, Flags::VOLATILE, ptr, off, MemAttrs::aligned(p.align));
                        s.b.store(Flags::NONE, v, at, off, MemAttrs::aligned(p.align));
                    }
                    Out::At(at)
                }
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
                // An aggregate: each piece of its bytes loaded from the value's place (aligned
                // as `T`) and stored `volatile`.
                Arg::Place(from) if volatile => {
                    let p = Pieces::of(l.size, align).ok_or(IntrinsicError::Unsupported { which: c.which, why: "a volatile store of an aggregate past 512 accesses" })?;
                    for off in p.offsets() {
                        let v = s.b.load(p.ty, Flags::NONE, from, off, MemAttrs::aligned(p.align));
                        s.b.store(Flags::VOLATILE, v, ptr, off, MemAttrs::aligned(p.align));
                    }
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
        // alloc's `write_box_via_move<T>(b: Box<MaybeUninit<T>>, x: T) -> Box<MaybeUninit<T>>`:
        // "Writes `x` into `b`." A `Box` of `Global` is its one pointer word.
        W::WriteBoxViaMove => {
            let id = s.layout(t0.ok_or_else(need_t)?)?;
            let l = *s.cx.layouts.get(id);
            let Some(ptr) = scalar(0) else { return Err(s.shape("a box that is not one pointer word (an allocator that is not zero-sized)")) };
            match c.args.get(1).copied().ok_or_else(shape)? {
                Arg::None => {}
                Arg::Scalar(v) => s.b.store(Flags::NONE, v, ptr, 0, MemAttrs::aligned(l.align)),
                Arg::Place(from) => {
                    let len = s.usize(l.size);
                    let mut attrs = MemAttrs::aligned(l.align);
                    attrs.src_align = l.align;
                    s.b.bulk(Op::Memcpy, Flags::NONE, ptr, from, len, attrs);
                }
            }
            Out::Scalar(ptr)
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
            // `<*const T>::offset_from`: "This function panics if `T` is a Zero-Sized Type"; both
            // methods assert `0 < pointee_size` before the call, so this one is never reached.
            if size == 0 {
                s.b.trap(ABORT_TRAP);
                return Ok(Out::Diverges);
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
                Site::Here(at) => Out::Scalar(location(&mut s, loc_ty, at)?),
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
        W::AssertInhabited | W::AssertZeroValid | W::AssertMemUninitializedValid => {
            return Err(s.shape("a validity assertion reached the intrinsic builder: the walk builds it (walk/validity.rs)"));
        }
        // core's `ub_checks()` and `overflow_checks()`: the building profile's answer, a
        // constant `bool` (the pinned core: "evaluation is delayed until monomorphization", so
        // never the body's `cfg!`, which is the LIBRARY crate's). Both settings are in every
        // code record's key (rule 21: blaze-ladder code_records.rs `Settings`).
        W::UbChecks => Out::Scalar(s.b.iconst(Ty::I8, u128::from(s.cx.ub_checks.on()))),
        W::OverflowChecks => Out::Scalar(s.b.iconst(Ty::I8, u128::from(s.cx.overflow_checks.on()))),
        // core/src/intrinsics/mod.rs: "this can only be called at compile-time as backends do
        // not have an implementation for it"; core's `type_name` wraps it in a `const` block.
        W::TypeName => return Err(s.shape("`type_name` reached the intrinsic builder: it is evaluated in its `const` block (blaze-consteval)")),
        // core reaches it in `TypeId::of`'s `const` block (blaze-consteval); a run-time call is
        // built from the same function, so it is the same 16 bytes.
        W::TypeId => {
            let t = t0.ok_or_else(need_t)?;
            let bits = match crate::instance::type_id_hash(t, s.cx.cx.ground.view(), s.cx.cx.scratch) {
                Ok(bits) => bits,
                Err(_) => return Err(s.shape("a type argument that is not a ground type (an alias, parameter or inference variable left)")),
            };
            let rid = s.layout(c.ret)?;
            let rl = *s.cx.layouts.get(rid);
            if rl.size != 16 {
                return Err(s.shape("a result that is not `TypeId`'s 16 bytes"));
            }
            let slot = s.b.slot(16, rl.align, SlotKind::Temp);
            let at = s.b.slot_addr(slot);
            for (off, word) in [(0, bits as u64), (8, (bits >> 64) as u64)] {
                let v = s.b.iconst(Ty::I64, u128::from(word));
                s.b.store(Flags::NONE, v, at, off, MemAttrs::aligned(rl.align.min(3)));
            }
            Out::At(at)
        }
        // The pinned core: "Converts with LLVM's fptoui/fptosi, which may return undef for values
        // out of range". Its rule (`f32::to_int_unchecked`): the value must not be NaN or
        // infinite, and must be representable in the integer type after truncation toward zero;
        // anything else is undefined behaviour. blaze's `fptosi`/`fptoui` saturate (NaN to 0),
        // a defined value where Rust's is undefined, so a refinement of the rule. f16 and f128
        // are made by blaze-lir's legalize (legalize/half.rs, quad.rs).
        W::FloatToIntUnchecked => {
            let (from, to) = (t0.ok_or_else(need_t)?, ty_arg(c, 1).ok_or_else(need_t)?);
            if !matches!(s.node(from), Some(PNode::Float(_))) {
                return Err(s.shape("a first type argument that is not a float"));
            }
            if !matches!(s.node(to), Some(PNode::Int(_) | PNode::Uint(_))) {
                return Err(s.shape("a second type argument that is not an integer"));
            }
            let x = scalar(0).ok_or_else(shape)?;
            let (ity, signed) = s.scalar(to)?;
            Out::Scalar(s.b.convert(if signed { Op::FptoSi } else { Op::FptoUi }, Flags::NONE, ity, x))
        }
        // `ptr_mask<T>(ptr: *const T, mask: usize) -> *const T`: "Masks out bits of the pointer
        // according to a mask." Safe to call, so nothing is promised: the address `and` the mask,
        // on the pointer's provenance (`<*const T>::mask`: "the returned pointer has the same
        // provenance").
        W::PtrMask => {
            let (Some(ptr), Some(mask)) = (scalar(0), scalar(1)) else { return Err(s.shape("a pointer that is not one word (`T` must be sized)")) };
            let addr = s.b.convert(Op::PtrAddr, Flags::NONE, Ty::I64, ptr);
            let kept = s.b.binary(Op::And, Flags::NONE, addr, mask);
            Out::Scalar(s.b.ptr_with_addr(ptr, kept))
        }
        // `raw_eq<T>(a: &T, b: &T) -> bool`: "Determines whether the raw bytes of the two values
        // are equal"; "It's UB to call this if any of the *bytes* in `*a` or `*b` are
        // uninitialized." Small values compare piece by piece ("comparing `i96`s instead of
        // forcing `alloca`s"), larger ones by `memcmp` ("Above some backend-decided threshold
        // this will emit calls to `memcmp`").
        W::RawEq => {
            let id = s.layout(t0.ok_or_else(need_t)?)?;
            let l = *s.cx.layouts.get(id);
            let (Some(a), Some(b)) = (scalar(0), scalar(1)) else { return Err(shape()) };
            let eq = match Pieces::of(l.size, l.align) {
                _ if l.size == 0 => s.b.iconst(Ty::I8, 1),
                Some(p) if p.count <= RAW_EQ_PIECES => {
                    let mut differ = None;
                    for off in p.offsets() {
                        let x = s.b.load(p.int, Flags::NONE, a, off, MemAttrs::aligned(p.align));
                        let y = s.b.load(p.int, Flags::NONE, b, off, MemAttrs::aligned(p.align));
                        let d = s.b.binary(Op::Xor, Flags::NONE, x, y);
                        let d = s.resize(d, Ty::I64, false);
                        differ = Some(match differ {
                            None => d,
                            Some(prev) => s.b.binary(Op::Or, Flags::NONE, prev, d),
                        });
                    }
                    let differ = differ.ok_or(s.shape("a sized value of no pieces"))?;
                    let zero = s.b.iconst(Ty::I64, 0);
                    let same = s.b.icmp(IntCond::Eq, differ, zero);
                    s.b.convert(Op::Zext, Flags::NONE, Ty::I8, same)
                }
                _ => {
                    let len = s.usize(l.size);
                    let mut attrs = MemAttrs::aligned(l.align);
                    attrs.src_align = l.align;
                    let order = s.b.memcmp(a, b, len, attrs);
                    let zero = s.b.iconst(Ty::I32, 0);
                    let same = s.b.icmp(IntCond::Eq, order, zero);
                    s.b.convert(Op::Zext, Flags::NONE, Ty::I8, same)
                }
            };
            Out::Scalar(eq)
        }
        // `compare_bytes(left: *const u8, right: *const u8, bytes: usize) -> i32`: "returning
        // negative if `left` is less, zero if all the bytes match, or positive if `left` is
        // greater"; "will usually lower to `memcmp`". LIR's `memcmp` is that `i32`.
        W::CompareBytes => {
            let (Some(a), Some(b), Some(len)) = (scalar(0), scalar(1), scalar(2)) else { return Err(shape()) };
            Out::Scalar(s.b.memcmp(a, b, len, MemAttrs::aligned(0)))
        }
        // blaze's `blaze_int_cast<S: Copy, D: Copy>(x: S) -> D`: "`x as D` between two integer
        // types of any width ... it truncates to `D`'s width, or extends by `S`'s sign"; "Both
        // types must be integers." The build's types are the standard widths (blaze-typeck
        // `IntTy`/`UintTy`), each an LIR integer of its width.
        W::BlazeIntCast => {
            let (from, to) = (t0.ok_or_else(need_t)?, ty_arg(c, 1).ok_or_else(need_t)?);
            for t in [from, to] {
                if !matches!(s.node(t), Some(PNode::Int(_) | PNode::Uint(_))) {
                    return Err(s.shape("a type argument that is not an integer (\"Both types must be integers.\")"));
                }
            }
            let x = scalar(0).ok_or_else(shape)?;
            let (_, signed) = s.scalar(from)?;
            let (dty, _) = s.scalar(to)?;
            Out::Scalar(s.resize(x, dty, signed))
        }
        // blaze's structural reflection intrinsics: each lowers to a compile-time constant by the
        // one rule (`field_count`, `records_disjoint`, `record_schema_id`) shared with
        // blaze-consteval.
        W::BlazeFieldCount => {
            let t = t0.ok_or_else(need_t)?;
            let count = field_count(t, s.cx.cx.ground.view(), s.cx.cx.facts).map_err(|e| match e {
                FieldCountError::MissingType => s.shape("a type argument not in the ground table"),
                FieldCountError::NoAdt(adt) => IntrinsicError::Layout(LayoutError { ty: t, why: crate::layout::Refusal::NoAdt(adt) }),
            })?;
            Out::Scalar(s.usize(count))
        }
        W::BlazeRecordsDisjoint => {
            let (a, b) = (t0.ok_or_else(need_t)?, ty_arg(c, 1).ok_or_else(need_t)?);
            let disjoint = records_disjoint(a, b, s.cx.cx.ground.view()).map_err(|e| match e {
                DisjointError::MissingType => s.shape("a type argument not in the ground table"),
                DisjointError::MissingRecordNames => s.shape("a structural record type without registered schema field names"),
            })?;
            Out::Scalar(s.b.iconst(Ty::I8, u128::from(disjoint)))
        }
        W::BlazeRecordSchemaId => {
            let t = t0.ok_or_else(need_t)?;
            let id = record_schema_id(t, s.cx.cx.ground.view()).ok_or(s.shape("a type argument not in the ground table"))?;
            Out::Scalar(s.b.iconst(Ty::I64, u128::from(id)))
        }
    };
    Ok(out)
}

/// The most accesses a volatile access of an aggregate is built as: each is one instruction, in
/// straight-line code.
pub const MOST_PIECES: u64 = 512;

/// The most piece pairs `raw_eq` compares in line before it calls `memcmp`.
const RAW_EQ_PIECES: u64 = 16;

/// The bytes of a value of `size` bytes at alignment `2^align` as `count` pieces of `width`
/// bytes each, every one aligned to its width: the widest of 8, 4, 2 and 1 bytes the alignment
/// allows (a size is a multiple of its alignment, so the pieces cover it exactly).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Pieces {
    /// The piece's LIR type to move it by: a pointer for 8 bytes, so a pointer word keeps its
    /// provenance through a volatile copy.
    pub ty: Ty,
    /// The integer of the piece's width, to compare it by.
    pub int: Ty,
    pub width: u64,
    /// Log2 of `width`: each access's alignment.
    pub align: u8,
    pub count: u64,
}

impl Pieces {
    /// `None` past [`MOST_PIECES`] pieces.
    pub fn of(size: u64, align: u8) -> Option<Pieces> {
        let align = align.min(3);
        let width = 1u64 << align;
        let count = size / width;
        if count > MOST_PIECES {
            return None;
        }
        let int = [Ty::I8, Ty::I16, Ty::I32, Ty::I64][usize::from(align)];
        let ty = if width == 8 { Ty::PTR } else { int };
        Some(Pieces { ty, int, width, align, count })
    }

    /// Each piece's byte offset (below 4 KiB: [`MOST_PIECES`] of at most 8 bytes).
    pub fn offsets(self) -> impl Iterator<Item = i32> {
        (0..self.count).map(move |k| (k * self.width) as i32)
    }
}

/// `compare_bytes`'s rule, and `raw_eq`'s at 0: "Lexicographically compare ... as unsigned
/// bytes, returning negative if `left` is less, zero if all the bytes match, or positive if
/// `left` is greater", as -1, 0 or 1. The constant evaluator computes it (blaze-consteval
/// machine.rs `intrinsic`); the builder's `memcmp` and piece compares are tested against it.
pub fn byte_order(left: &[u8], right: &[u8]) -> i32 {
    match left.cmp(right) {
        core::cmp::Ordering::Less => -1,
        core::cmp::Ordering::Equal => 0,
        core::cmp::Ordering::Greater => 1,
    }
}

/// `blaze_int_cast`'s rule: the bits `x` of an integer of `from_bits` bits (signed when
/// `from_signed`) as an integer of `to_bits` bits: "it truncates to `D`'s width, or extends by
/// `S`'s sign". Every width from 1 to 128 (the arbitrary ones too). The constant evaluator
/// computes it; the builder's `trunc`/`sext`/`zext` are tested against it.
pub fn int_cast_bits(x: u128, from_bits: u32, from_signed: bool, to_bits: u32) -> u128 {
    let keep = |v: u128, bits: u32| if bits >= 128 { v } else { v & ((1u128 << bits) - 1) };
    let x = keep(x, from_bits);
    let sign = from_bits > 0 && from_bits < 128 && from_signed && (x >> (from_bits - 1)) & 1 == 1;
    let wide = if sign { x | !((1u128 << from_bits) - 1) } else { x };
    keep(wide, to_bits)
}

/// Why [`field_count`] could not answer for a ground type.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FieldCountError {
    /// The type is not in the ground table.
    MissingType,
    /// `T` is an ADT whose definition is not loaded in `facts` yet.
    NoAdt(ItemKey),
}

/// `blaze_field_count::<T>()`'s rule: the field count of `T` as a `usize`:
/// - `PNode::Record(_, fields)` -> `fields.len`
/// - `PNode::Tuple(fields)` -> `fields.len`
/// - `PNode::Adt(adt_key, _)` -> if struct (`AdtKind::Struct` with one variant), its field count;
///   otherwise `0` (for enums/unions)
/// - any other type -> `0`.
pub fn field_count(ty: PTy, tys: PTys<'_>, facts: &dyn LayoutFacts) -> Result<u64, FieldCountError> {
    let node = tys.get(ty).copied().ok_or(FieldCountError::MissingType)?;
    match node {
        PNode::Record(_, fields) | PNode::Tuple(fields) => Ok(u64::from(fields.len)),
        PNode::Adt(adt_key, _) => {
            let adt = facts.adt(adt_key).ok_or(FieldCountError::NoAdt(adt_key))?;
            match (adt.kind, adt.variants) {
                (AdtKind::Struct, [variant]) => Ok(u64::from(variant.fields.len)),
                _ => Ok(0),
            }
        }
        _ => Ok(0),
    }
}

/// Why [`records_disjoint`] could not answer for two ground types.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DisjointError {
    /// A type argument is not in the ground table.
    MissingType,
    /// A non-empty structural record type has no registered schema field names in `tys`.
    MissingRecordNames,
}

/// `blaze_records_disjoint::<A, B>()`'s rule:
/// - if `A` and `B` are both `PNode::Record`: `true` iff their canonical field-name sets share
///   zero `Name` elements
/// - if either is the empty tuple `()`: `true`
/// - otherwise: `a != b`.
pub fn records_disjoint(a: PTy, b: PTy, tys: PTys<'_>) -> Result<bool, DisjointError> {
    let na = tys.get(a).copied().ok_or(DisjointError::MissingType)?;
    let nb = tys.get(b).copied().ok_or(DisjointError::MissingType)?;
    match (na, nb) {
        (PNode::Record(sa, fa), PNode::Record(sb, fb)) => {
            if fa.len == 0 || fb.len == 0 {
                return Ok(true);
            }
            let names_a = tys.record_names(sa).ok_or(DisjointError::MissingRecordNames)?;
            let names_b = tys.record_names(sb).ok_or(DisjointError::MissingRecordNames)?;
            Ok(!names_a.iter().any(|name| names_b.contains(name)))
        }
        (PNode::Tuple(l), _) | (_, PNode::Tuple(l)) if l.len == 0 => Ok(true),
        _ => Ok(a != b),
    }
}

/// `blaze_record_schema_id::<T>()`'s rule: `schema.0` for `PNode::Record(schema, _)`, `0` for any
/// other type.
pub fn record_schema_id(ty: PTy, tys: PTys<'_>) -> Option<u64> {
    let node = tys.get(ty).copied()?;
    Some(match node {
        PNode::Record(schema, _) => schema.0,
        _ => 0,
    })
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
pub fn resize_in<LA: Allocator + Clone, S: Sink>(b: &mut Builder<LA, S>, v: Value, to: Ty, signed: bool) -> Value {
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
pub fn read_discriminant<LA: Allocator + Clone, S: Sink, A: Allocator + Clone>(
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

/// A `Location { filename, line, col }` as a data row of the function; its address. The row
/// is the filename's wide pointer (an address relocation to a row of the path's bytes and a
/// NUL, core's `Location::file_as_c_str`, then the path's length) and, at the `line` and `col`
/// fields, a source position each of `at` (its item and item-relative byte: rule 71), which
/// placement fills from the item's present place (blaze-lir `DataPositionRec`). The bytes there
/// stay zero, so a line inserted above the item moves no record.
fn location<LA: Allocator + Clone, S: Sink, A: Allocator + Clone>(
    s: &mut B<'_, '_, '_, '_, LA, S, A>,
    loc_ty: PTy,
    at: crate::CallerSite<'_>,
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
    if at.file.len() >= 256 {
        return Err(IntrinsicError::Unsupported { which: s.which, why: "a `Location` of a path of 256 bytes or more" });
    }
    if l.size > 64 {
        return Err(s.shape("a `Location` past 64 bytes"));
    }
    let len = at.file.len();
    let mut path = [0u8; 256];
    path[..len].copy_from_slice(at.file.as_bytes());
    let path = s.b.data(&path[..len + 1], 0, &[]);
    let mut row = [0u8; 64];
    let size = l.size as usize;
    let f = f as usize;
    row[f + 8..f + 16].copy_from_slice(&(len as u64).to_le_bytes());
    let relocs = [blaze_lir::builder::DataReloc { offset: f as u32, target: path, addend: 0 }];
    let line = blaze_lir::builder::DataPosition { offset: ln as u32, item: at.item.0, site: at.offset, field: blaze_lir::PositionField::Line };
    let col = blaze_lir::builder::DataPosition { offset: cl as u32, item: at.item.0, site: at.offset, field: blaze_lir::PositionField::Col };
    // By offset, as the verifier asks.
    let positions = if ln < cl { [line, col] } else { [col, line] };
    let r = s.b.data_with_positions(&row[..size], u32::from(l.align), &relocs, &positions);
    Ok(s.b.data_addr(r))
}

mod element;
pub mod simd;
mod simd_vec;
pub mod target;
pub mod validity;
#[cfg(test)]
mod tests;

pub use element::ElementArg;
