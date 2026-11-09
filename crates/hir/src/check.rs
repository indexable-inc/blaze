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
//! more (docs/v2/plan/design/frontend.md), a slot is written once (no lock),
//! and unlike rustc's the diagnostics come out in one order whatever the threads.

pub mod bodies;

use base_db::Db;
use hir_def::{DefWithBodyId, GenericDefId, Lookup, nameres::crate_def_map};

use self::bodies::Plan;
use crate::{
    AnyDiagnostic, Crate, DefWithBody, Module, TypeOwnerId,
    diagnostics::{DiagnosticsCollector, ForeignTraitImpl},
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

/// What [`check_sealed`] gives of a crate.
pub struct SealedCheck<'db> {
    /// `hir-ty-coherence`'s refusals.
    pub diagnostics: Vec<AnyDiagnostic<'db>>,
    /// Each impl of another crate's trait written in its source (a builtin derive's aside), its
    /// modules' and its bodies' blocks', in the def maps' order: for rustc's orphan check
    /// (blaze-diagnostics' `trait_impl_orphan`), whose refusals alone are errors.
    pub foreign_impls: Vec<ForeignTraitImpl>,
}

/// blaze: the `#[blaze::sealed]` checks of `krate` alone, `hir-ty-coherence`'s, with the impls
/// of other crates' traits that rustc's orphan check is to run on: for a crate of which nothing
/// else is checked (a dependency of which only what the build's programs reach is,
/// `blaze-cargo`'s `reached_only`) whose graph has a sealed trait in it
/// ([`hir_ty::sealed_in_graph`]). Sealing is the sealing crate's opt-in, and binds every crate
/// that can name its trait, a registry crate rustc accepted included
/// (docs/v2/plan/design/sealed-orphan.md). Empty, with one flag read, for a crate with
/// no sealed trait in its graph.
pub fn check_sealed(db: &Db, krate: Crate) -> SealedCheck<'_> {
    // alloc: the results; empty, so no allocation, unless the graph has a sealed trait
    let (mut diagnostics, mut foreign_impls) = (Vec::new(), Vec::new());
    if hir_ty::sealed_in_graph(db, krate.id) {
        DiagnosticsCollector::collect_sealed(
            db,
            krate.id,
            &mut diagnostics,
            &mut foreign_impls,
            &every_body,
        );
    }
    SealedCheck { diagnostics, foreign_impls }
}

/// What [`check_expansions`] gives of a crate.
#[derive(Default)]
pub struct ExpansionCheck<'db> {
    /// The expansion errors of the items its modules declare outside any body, module by
    /// module in the def map's order.
    pub items: Vec<AnyDiagnostic<'db>>,
    /// Each body that lowering found anything in, with what it found: its macro calls'
    /// expansion errors and the errors of the items its blocks declare among them. In the def
    /// map's order, each body before those declared in it.
    pub bodies: Vec<(DefWithBodyId, Vec<AnyDiagnostic<'db>>)>,
}

impl ExpansionCheck<'_> {
    /// Whether expanding and lowering the crate found nothing.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty() && self.bodies.is_empty()
    }
}

/// blaze: what expanding the macro calls of `krate` and lowering its bodies finds, nothing
/// type-checked: for a crate of which nothing else is checked (a dependency of which only what
/// the build's programs reach is, `blaze-cargo`'s `reached_only`). rustc expands every macro
/// call of every crate it compiles, so a call that fails to expand there is an error of that
/// crate, reached or not; found only where a program reaches the body, it was a later crate's
/// "MIR that failed to lower" with no call in it, the crate that holds it built. Every body is
/// lowered, the crate's roots split over the database's workers; what they found is then read
/// in the def map's order, so the result is the same whatever the threads.
pub fn check_expansions(db: &Db, krate: Crate) -> ExpansionCheck<'_> {
    let _p = tracing::info_span!(target: "blaze", "expansion_check", krate = ?krate.id).entered();
    let roots = hir_def::nameres::demand::crate_body_owners(db, krate.id);
    // Each root's body, those declared in it, and their macro calls' expansions, lowered in
    // parallel: what the walk below reads is then in the database.
    db.run_all(blaze_mem::Tag::Expand, roots.len(), |db, at| {
        // alloc: one root a task, its walk's queue
        hir_def::nameres::demand::walk_bodies(db, vec![roots[at]], |_| true, |_, _| {});
    });
    // alloc: the results; empty, so no allocation, unless a diagnostic is reported
    let mut items = Vec::new();
    DiagnosticsCollector::collect_item_expansions(db, krate.id, &mut items, &every_body);
    // alloc: as above
    let mut bodies = Vec::new();
    hir_def::nameres::demand::walk_bodies(
        db,
        roots,
        |_| true,
        |owner, _| {
            // alloc: as above
            let mut found = Vec::new();
            DiagnosticsCollector::collect_body_expansions(db, owner, &mut found, &every_body);
            if !found.is_empty() {
                bodies.push((owner, found));
            }
        },
    );
    ExpansionCheck { items, bodies }
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
    DiagnosticsCollector::collect_body(
        db,
        def,
        TypeOwnerId::GenericDefId(owner),
        &mut acc,
        style_lints,
        &every_body,
    );
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
pub fn check_workspace(db: &Db, style_lints: bool) -> Vec<(Crate, Vec<AnyDiagnostic<'_>>)> {
    check_crates(db, &workspace_crates(db), style_lints)
}
