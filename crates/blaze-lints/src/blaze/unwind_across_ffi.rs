//! `unwind_across_ffi`: a function defined in Rust with a foreign ABI that can't unwind
//! (`extern "C"`, `extern "system"`, `extern "sysv64"`, .., none of the `-unwind` ones), whose
//! body can panic and never calls `std::panic::catch_unwind`. A panic can't unwind out of such a
//! function: the process aborts at its boundary (before Rust 1.81 it was undefined behavior),
//! and the foreign caller never sees an error it could handle.
//!
//! Can panic is read from the body alone, not from what it calls ([`general_facts::panic_sites`]):
//! `unwrap`, `expect`, `unwrap_err` and `expect_err` of `Option` and `Result`; `panic!`,
//! `assert!`, `unreachable!`, `todo!` and `unimplemented!`; `panic_any` and `resume_unwind`; an
//! index; and an integer `/` or `%` by a divisor that is not a nonzero literal. Each is a finding.
//!
//! Narrowed: overflow of `+`, `-` and `*` is not counted (it panics only with overflow checks,
//! and nearly every FFI shim does arithmetic on lengths), nor is a call of a function whose body
//! may panic (no interprocedural walk). A body that calls `catch_unwind` anywhere is taken as
//! catching every panic of it.

use hir_def::{DefWithBodyId, signatures::FunctionSignature};

use super::{Finding, Lint, facts, general_facts};
use crate::check::Cx;

const LINT: Lint = Lint::UnwindAcrossFfi;

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(DefWithBodyId::FunctionId(f)) = DefWithBodyId::try_from(analysis.def()) else {
        return found;
    };
    let db = cx.db();
    let signature = FunctionSignature::of(db, f);
    let abi = signature.abi;
    if abi.is_rustic_abi() || abi.as_str().ends_with("-unwind") {
        return found;
    }
    if general_facts::calls_std(db, analysis, "panic", "catch_unwind") {
        return found;
    }
    let name = signature.name.as_str();
    let abi = abi.as_str();
    for site in general_facts::panic_sites(db, analysis) {
        let Some(range) = analysis.expr_range(site.expr) else { continue };
        let message = format!(
            "{} can panic inside `extern \"{abi}\" fn {name}`, and a panic can't unwind out of it: the process aborts",
            site.what
        );
        let note = format!(
            "a panic reaching the boundary of an `extern \"{abi}\"` function aborts instead of unwinding into the \
             foreign caller; catch it with `std::panic::catch_unwind` and return an error code, or declare the \
             function with the ABI's `-unwind` form (`extern \"C-unwind\"`) when the caller is built to unwind"
        );
        facts::push(&mut found, LINT, range, message, Some(note));
    }
    found
}
