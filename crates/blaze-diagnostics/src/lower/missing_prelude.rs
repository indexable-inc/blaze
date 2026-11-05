//! The crate's injected `extern crate std;` (`core` under `#![no_std]`) that names no
//! dependency: rustc's E0463 "can't find crate for `std`"; or that crate with no
//! `prelude::rust_20xx`: E0432 on the injected `use`. Neither has a span: rustc's injected
//! items have none.
//!
//! Follows rustc's `compiler/rustc_metadata/src/errors.rs` (`CannotFindCrate`) and
//! `compiler/rustc_builtin_macros/src/standard_library_imports.rs` (MIT or Apache-2.0).

use hir_def::nameres::diagnostics::PreludeError;

use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::{Code, Diag, Level},
};

pub(crate) fn lower(_cx: &Cx<'_, '_>, d: &hir::MissingPrelude) -> Vec<Found> {
    let name = d.krate.as_str();
    let (code, message) = match d.error {
        // alloc: the error's text, for an error hir found
        PreludeError::CrateNotFound => ("E0463", format!("can't find crate for `{name}`")),
        // alloc: the error's text, for an error hir found
        PreludeError::NoPreludeModule => ("E0432", format!("unresolved import `{name}::prelude`")),
    };
    // alloc: the error's code, for an error hir found
    let diag = Diag::new(Level::Error, Some(Code::Error(code.to_owned())), message);
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Resolve)]
}
