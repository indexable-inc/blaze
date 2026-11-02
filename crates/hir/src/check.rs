//! What `blaze check` checks (#1293): every item and every body of the workspace's crates, as
//! rustc checks every crate it compiles, with their borrow checks.
//!
//! Nothing here asks for a dependency's bodies. Of a dependency, checking a workspace body
//! infers only what that body reaches through what a body leaks into its interface: an opaque
//! type's hidden type and its auto traits, a constant it evaluates, a `const fn` it calls in a
//! const context. A build reaches the rest of a dependency's bodies only through codegen of the
//! instances it compiles.
//!
//! The work is one plane task per body and one per file module for its items' checks
//! ([`Plan`]): each task's diagnostics go in its own slot, and the result is read in the def
//! maps' order, so it is the same whatever order the tasks ran in. A build tells the plan the
//! bodies its codegen reaches, which are checked first ([`Plan::reached`]).
//!
//! The technique to match is rustc's parallel front end, `par_hir_body_owners` on rayon's work
//! stealing: one body per task. A task costs about 200 ns to schedule against a body's 11 µs or
//! more (docs/v2/plan/lanes/frontend/fast-bodies-tasks.md), a slot is written once (no lock),
//! and unlike rustc's the diagnostics come out in one order whatever the threads.

pub mod bodies;

use base_db::Db;
use hir_def::{DefWithBodyId, GenericDefId, Lookup, nameres::crate_def_map};

use self::bodies::Plan;
use crate::{
    AnyDiagnostic, Crate, DefWithBody, Module, TypeOwnerId, diagnostics::DiagnosticsCollector,
};

/// The body filter of a check that asks for every body.
pub fn every_body(_: DefWithBody) -> bool {
    true
}

/// The crates `blaze check` checks: the workspace's members.
pub fn workspace_crates(db: &Db) -> Vec<Crate> {
    // alloc: once a check, its result, a word per crate
    Crate::all(db).into_iter().filter(|it| it.origin(db).is_local()).collect()
}

/// The modules of `krate` a check takes one at a time: every module but the inline ones, which
/// are checked with the module they are written in, in the def map's order.
pub fn file_modules(db: &Db, krate: Crate) -> Vec<Module> {
    crate_def_map(db, krate.id)
        .modules(db)
        .filter(|(_, data)| !data.origin.is_inline())
        .map(|(id, _)| Module { id })
        // alloc: once a crate, its result, a word per file module
        .collect()
}

/// The diagnostics of `module`, of the inline modules written in it, and of every item and
/// body declared in them, borrow checks included; with `bodies`, of the bodies it accepts only
/// (a driver that knows which bodies an edit left as they were checked). It asks queries of
/// the database and keeps nothing.
pub fn check_module<'db>(
    db: &'db Db,
    module: Module,
    style_lints: bool,
    bodies: &(impl Fn(DefWithBody) -> bool + ?Sized),
) -> Vec<AnyDiagnostic<'db>> {
    // alloc: the result; empty, so no allocation, unless a diagnostic is reported
    let mut acc = Vec::new();
    DiagnosticsCollector::collect(db, module.id, &mut acc, style_lints, bodies);
    acc
}

/// The diagnostics of body `def` alone, as a [`Plan`]'s task for it checks it: inference,
/// borrow, match and unsafety checks, without its module's item checks. An enum variant's
/// discriminant is checked in the generics of its enum, as its module's check does.
pub fn check_body(db: &Db, def: DefWithBodyId, style_lints: bool) -> Vec<AnyDiagnostic<'_>> {
    let owner = match def {
        DefWithBodyId::FunctionId(it) => GenericDefId::from(it),
        DefWithBodyId::ConstId(it) => GenericDefId::from(it),
        DefWithBodyId::StaticId(it) => GenericDefId::from(it),
        DefWithBodyId::VariantId(it) => GenericDefId::from(it.lookup(db).parent),
    };
    // alloc: the result; empty, so no allocation, unless a diagnostic is reported
    let mut acc = Vec::new();
    DiagnosticsCollector::collect_body(db, def, TypeOwnerId::GenericDefId(owner), &mut acc, style_lints, &every_body);
    acc
}

/// [`check_module`] of each of [`file_modules`] of `krate`, in their order, as one [`Plan`].
pub fn check_crate<'db>(
    db: &'db Db,
    krate: Crate,
    style_lints: bool,
    bodies: &(impl Fn(DefWithBody) -> bool + Sync + ?Sized),
) -> Vec<AnyDiagnostic<'db>> {
    Plan::new(db, &[krate], bodies).run(db, style_lints).pop().unwrap_or_default()
}

/// [`check_crate`] of each of `crates`, in their order, all in one [`Plan`].
pub fn check_crates<'db>(
    db: &'db Db,
    crates: &[Crate],
    style_lints: bool,
) -> Vec<(Crate, Vec<AnyDiagnostic<'db>>)> {
    let found = Plan::new(db, crates, &every_body).run(db, style_lints);
    // alloc: once a check, its result, a pair per crate
    crates.iter().copied().zip(found).collect()
}

/// [`check_crates`] of [`workspace_crates`].
pub fn check_workspace(
    db: &Db,
    style_lints: bool,
) -> Vec<(Crate, Vec<AnyDiagnostic<'_>>)> {
    check_crates(db, &workspace_crates(db), style_lints)
}