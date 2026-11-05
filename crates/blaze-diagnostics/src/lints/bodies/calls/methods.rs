//! rustc's late lint passes of method calls, panics and `asm!` in bodies.
//!
//! - `noop_method_call`: `NoopMethodCall` (`noop_method_call`, `suspicious_double_ref_op`).
//! - `map_unit_fn`: `MapUnitFn` (`map_unit_fn`).
//! - `non_fmt_panic`: `NonPanicFmt` (`non_fmt_panics`).
//! - `asm_labels`: `AsmLabels` (`named_asm_labels`, `binary_asm_labels`).
//!
//! Follows rustc's `compiler/rustc_lint/src/lib.rs` (`BuiltinCombinedLateLintModPass`) (MIT or
//! Apache-2.0).

pub(crate) mod asm_labels;
pub(crate) mod map_unit_fn;
pub(crate) mod non_fmt_panic;
pub(crate) mod noop_method_call;
