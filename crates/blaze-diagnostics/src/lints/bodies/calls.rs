//! rustc's late lint passes of calls, statements, `let`s and patterns in bodies.
//!
//! - `statements`: `PathStatements`, `LetUnderscore`, `UnusedResults`, `UnitBindings`,
//!   `NonShorthandFieldPatterns`, `UnusedAllocation`.
//! - `items`: `DropForgetUseless`, `InvalidFromUtf8`, `InvalidAtomicOrdering`.
//! - `desugar`: `ForLoopsOverFallibles`, `ShadowedIntoIter`, `IfLetRescope`.
//! - `methods`: `NoopMethodCall`, `MapUnitFn`, `NonPanicFmt`, `AsmLabels`.
//!
//! Follows rustc's `compiler/rustc_lint/src/lib.rs` (`BuiltinCombinedLateLintModPass`) (MIT or
//! Apache-2.0).

pub(super) mod desugar;
pub(super) mod items;
pub(super) mod methods;
pub(super) mod statements;
