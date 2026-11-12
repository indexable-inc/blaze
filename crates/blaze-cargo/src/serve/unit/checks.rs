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
//!
//! A [`Plan`] runs once, and a unit's check may run more than once: the phase gives way where
//! going on could not end (a cycle of waits, memory, [`blaze_graph::Cancelled`]) and is posted
//! again whole, in the plan's run or after it (the item checks and lints of
//! [`blaze_diagnostics::check_planned`] wait too). So the plan is the [`Checking`]'s to make:
//! each run after the first makes its own, from the check the one before kept or, where that
//! one unwound, from the check it was made from, which its drop puts back. What codegen
//! reached before is not told to the new plan: it orders the check and changes none of it.
//! The same holds for a first run that begins in a later view than the one the [`Checking`]
//! was made in: a plan that keeps runs only in its own view ([`Plan::made_in`]).

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use base_db::Db;
use blaze_core::lock::Lock;
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
    /// The plan of the run under way, or of the next; none only while a run after the first
    /// makes its own. Locked to read or leave the field, never across the plan's work.
    plan: Lock<Option<Arc<EveryBody>>>,
    /// A run of the check began: its plan is spent, and one after it makes another.
    begun: AtomicBool,
    failed: AtomicBool,
}

/// A plan of every body of a crate ([`Plan::of_every_body`]).
type EveryBody = Plan<'static, fn(DefWithBody) -> bool>;

impl Checking {
    /// The check of every body of `krate`, listed. What a body's or a module's check found
    /// clean is the database's memo ([`Plan::of_every_body`]), so the build after an edit
    /// checks the bodies the edit changed something for and reads nothing of the others.
    pub(crate) fn new(db: &Db, krate: hir::Crate) -> Checking {
        // alloc: once per unit checked, the plan its check and its codegen share
        let plan = Arc::new(Plan::of_every_body(db, &[krate], false));
        Checking {
            krate,
            plan: Lock::new("unit.checking.plan", Some(plan)),
            begun: AtomicBool::new(false),
            failed: AtomicBool::new(false),
        }
    }

    /// The check's diagnostics and whether they break its items ([`CrateCheck`]), once every
    /// body is checked; a codegen beside it is told, from then, whether it failed.
    ///
    /// One run at a time, as a unit's [`Phase::Bodies`](crate::serve::work::Phase) is one
    /// task. A run after one that began (it gave way and was posted again), and a first run
    /// in a later view than `new`'s, checks with a plan of its own, made here in the view it
    /// runs in: the module says why.
    pub(crate) fn check(&self, db: &Db, files: Files<'_>, options: &Options) -> CrateCheck {
        // The plan `new` made is of the view the unit's names were done in. Its check starts
        // when a worker takes it, and by then the database may be at a later view (another
        // unit's sources were set: a cold build of a large workspace does it between the two
        // for some unit), where that plan does not run: what it visits is the changed set up
        // to its own view.
        let made = match self.begun.swap(true, Ordering::AcqRel) {
            true => None,
            // clone: a refcount, once per unit checked
            false => self.plan.lock().clone().filter(|plan| plan.made_in(db)),
        };
        let plan = match made {
            Some(plan) => plan,
            None => {
                // The spent or stale plan goes first, so the kept check it holds, if its run
                // unwound or never began, is back before the next plan asks for it.
                let spent = self.plan.lock().take();
                drop(spent);
                // alloc: once per run of a unit's check posted again or begun in a later view
                let plan = Arc::new(Plan::of_every_body(db, &[self.krate], false));
                // clone: a refcount, once per such run
                *self.plan.lock() = Some(plan.clone());
                plan
            }
        };
        let check = blaze_diagnostics::check_planned(db, self.krate, &*plan, files, options);
        if failed(&check.diags) {
            self.failed.store(true, Ordering::Release);
        }
        check
    }
}

impl BodyChecks for Checking {
    fn reached(&self, bodies: &[DefWithBodyId]) {
        // clone: a refcount per batch of bodies codegen's walk reaches, so the plan's index
        // is made and its ring written outside the lock
        let plan = self.plan.lock().clone();
        // None while a run posted again makes its plan: the order of a check, not its outcome.
        if let Some(plan) = plan {
            plan.reached(bodies);
        }
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
