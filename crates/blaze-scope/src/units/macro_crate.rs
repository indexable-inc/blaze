//! Which units of the graph are a proc-macro crate's code, and so what each unit is given
//! without naming it: the one rule of the list (rule 30).
//!
//! The Reference, "Procedural macros": "Procedural macro crates ... implicitly link to the
//! compiler-provided `proc_macro` crate", which is in their extern prelude with no
//! `extern crate`. The unit graph makes TWO units of a proc-macro library when tests are asked
//! (`blaze check` asks them): the proc macro itself (`UnitKind::ProcMacro`) and its test
//! harness (`UnitKind::Test { bin: None }`, the same root file). The harness compiles the same
//! text (`use proc_macro::TokenStream;` at its head), so it is given what the proc macro is
//! given (cargo passes `--extern proc_macro` to every unit of a proc-macro target, its test
//! too). Before this rule the harness was given a plain library's prelude: on the two-member
//! workspace pm-small, `blaze check` printed "error[unresolved] mac/src/lib.rs:1:5 no
//! `proc_macro` in this scope" for the harness alone, while the proc macro resolved.
//!
//! Sans-IO, like its parent: nothing here reads a file.

#![blaze::sans_io]

use blaze_unit_graph::{Implicit, Unit, UnitKind, implicit};

/// Whether `units[at]` compiles a proc-macro library's code: the proc macro itself, or its
/// library's test harness (a `Test { bin: None }` unit of a package whose library is a proc
/// macro: the graph makes that package's library unit `UnitKind::ProcMacro`). A binary's test
/// harness is a binary's code, never a proc macro's.
pub(super) fn is_macro_crate(units: &[Unit<'_>], at: usize) -> bool {
    let u = &units[at];
    match &u.kind {
        UnitKind::ProcMacro => true,
        UnitKind::Test { bin: None } => units.iter().any(|v| v.package == u.package && v.kind == UnitKind::ProcMacro),
        _ => false,
    }
}

/// What `units[at]` is given without naming it (blaze-unit-graph's `implicit`), a proc macro's
/// library test harness given what the proc macro is: `proc_macro` in its extern prelude, and
/// the macro side's Rust `std` (a proc macro is never a blaze-edition crate's bstd user).
pub(super) fn unit_implicit(units: &[Unit<'_>], at: usize) -> Implicit {
    let u = &units[at];
    if is_macro_crate(units, at) { implicit(u.edition, &UnitKind::ProcMacro) } else { implicit(u.edition, &u.kind) }
}
