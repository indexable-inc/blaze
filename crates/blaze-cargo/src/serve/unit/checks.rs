//! A unit's check beside its codegen (docs/v2/plan/design/frontend.md).
//!
//! The check is the unit's [`Phase::Bodies`](crate::serve::work::Phase), its codegen
//! [`Phase::Codegen`](crate::serve::work::Phase), two nodes of the plane that run side by side
//! once the unit's names are done, sharing one [`Checking`]. The check's bodies are a [`Plan`]
//! that codegen's walk tells what it reaches ([`BodyChecks::reached`]), so the bodies the build
//! compiles are checked before the others. Which bodies are checked does not change: the check
//! is of every body of the crate, and the unit's outcome waits for all of them, as rustc's does.
//! Once the check fails, codegen stops at its next step ([`BodyChecks::stopped`]) and what it
//! made is dropped: the unit's outcome is the check's errors.

use std::sync::atomic::{AtomicBool, Ordering};

use base_db::Db;
use blaze_diagnostics::{CrateCheck, Diag, Files, Options};
use hir::{DefWithBody, check::bodies::Plan};
use hir_def::DefWithBodyId;
use hir_ty_codegen::v2::BodyChecks;

use super::compile::cargo_hides;

#[cfg(test)]
mod tests;

/// The check of a unit's crate, and whether it failed. See the module.
pub(crate) struct Checking {
    krate: hir::Crate,
    plan: Plan<'static, fn(DefWithBody) -> bool>,
    failed: AtomicBool,
}

impl Checking {
    /// The check of every body of `krate`, listed. What a body's or a module's check found
    /// clean is the database's memo ([`Plan::of_every_body`]), so the build after an edit
    /// checks the bodies the edit changed something for and reads nothing of the others.
    pub(crate) fn new(db: &Db, krate: hir::Crate) -> Checking {
        let plan = Plan::of_every_body(db, &[krate], false);
        Checking { krate, plan, failed: AtomicBool::new(false) }
    }

    /// The check's diagnostics and whether they break its items ([`CrateCheck`]), once every
    /// body is checked; a codegen beside it is told, from then, whether it failed.
    pub(crate) fn check(&self, db: &Db, files: Files<'_>, options: &Options) -> CrateCheck {
        let check = blaze_diagnostics::check_planned(db, self.krate, &self.plan, files, options);
        if failed(&check.diags) {
            self.failed.store(true, Ordering::Release);
        }
        check
    }
}

impl BodyChecks for Checking {
    fn reached(&self, bodies: &[DefWithBodyId]) {
        self.plan.reached(bodies);
    }

    fn stopped(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }
}

/// Whether a check's diagnostics fail its unit: an error cargo shows, as the unit's outcome
/// counts them.
fn failed(diags: &[Diag]) -> bool {
    diags.iter().any(|it| it.is_error() && !cargo_hides(&it.message))
}
