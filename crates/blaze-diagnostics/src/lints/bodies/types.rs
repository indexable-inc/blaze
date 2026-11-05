//! rustc's late lint passes of values, numbers, pointers and casts in bodies.
//!
//! - `limits`: `TypeLimits` (`overflowing_literals`, `unused_comparisons`,
//!   `invalid_nan_comparisons`, `ambiguous_wide_pointer_comparisons`,
//!   `unpredictable_function_pointer_comparisons`).
//! - `values`: `InvalidValue`, `EnumIntrinsicsNonEnums`, `MutableTransmutes`, `CheckTransmutes`,
//!   `InteriorMutableConsts`.
//! - `pointers`: `DerefNullPtr`, `InvalidReferenceCasting`, `ImplicitAutorefs`, `PtrNullChecks`,
//!   `DanglingPointers`.
//! - `casts`: `StaticMutRefs`, `FunctionCastsAsInteger`, `ImplicitProvenanceCasts`,
//!   `RawBorrowsViaReferences`.
//! - `util`: what they share.
//!
//! Follows rustc's `compiler/rustc_lint/src/lib.rs` (`BuiltinCombinedLateLintModPass`) (MIT or
//! Apache-2.0).

pub(super) mod casts;
pub(super) mod limits;
pub(super) mod pointers;
pub(crate) mod util;
pub(super) mod values;
