//! What of a body may become code that runs.
//!
//! A build's output never holds code whose borrow check failed or did not run (cargo's rule: a
//! borrowck error means no binary). The only way to MIR for a build's codegen is [`checked`],
//! which gives a [`Checked`] once the borrow check of the body, and of every closure, coroutine
//! and `const` block in it, found no error; [`CheckedMir`] is made from nothing else.
//!
//! Live patching alone may have code before the check finishes (#1302, #276): [`speculate`] is
//! its entry point, and gives an [`Unchecked`], whose MIR is an [`UncheckedMir`] that no
//! [`CheckedMir`] can be made of, and a [`Pending`] check. The patch applies the code, finishes
//! the check beside it (a plane task sharing the database by reference) and rolls
//! back when [`Pending::finish`] refuses. A build's codegen takes [`CheckedMir`] and a build's
//! reuse of stored code takes a [`Checked`], so nothing the live path made can reach one by its
//! types.

use std::{fmt, marker::PhantomData};

use base_db::Db;
use hir_def::{DefWithBodyId, ExpressionStoreOwnerId};
use hir_ty_mir::mir::{MirBody, MirLowerError};

use crate::{BorrowckDiagnostic, BorrowckResult, NotChecked, borrowck};

/// Proof that the borrow check of a body passed, in the database revision `'db` it was asked
/// in: an edit, which needs the database mutably, ends it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Checked<'db> {
    owner: DefWithBodyId,
    _revision: PhantomData<fn() -> &'db ()>,
}

/// Why a body's code may not run.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Refused {
    /// The borrow check found these errors, rustc's, in the body or a body in it.
    Errors { owner: DefWithBodyId, errors: Vec<BorrowckDiagnostic> },
    /// The checker stopped at an error rustc reports before borrow checking.
    NotChecked { owner: DefWithBodyId, why: NotChecked },
    /// MIR lowering does not support something in the body or a body in it, so it was not
    /// checked.
    Unsupported { owner: DefWithBodyId, what: String },
    /// The MIR asked for could not be built.
    Lowering { owner: DefWithBodyId, error: String },
    /// The MIR given is of another body than the one checked.
    NotOf { owner: DefWithBodyId, mir_owner: String },
}

impl Refused {
    pub fn owner(&self) -> DefWithBodyId {
        match *self {
            Refused::Errors { owner, .. }
            | Refused::NotChecked { owner, .. }
            | Refused::Unsupported { owner, .. }
            | Refused::Lowering { owner, .. }
            | Refused::NotOf { owner, .. } => owner,
        }
    }
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::Errors { owner, errors } => {
                write!(f, "the borrow check of {owner:?} found {} error(s):", errors.len())?;
                for error in errors {
                    write!(f, " {}: {};", error.code, error.message)?;
                }
                Ok(())
            }
            Refused::NotChecked { owner, why } => {
                write!(f, "{owner:?} was not borrow checked: {why:?}")
            }
            Refused::Unsupported { owner, what } => {
                write!(f, "{owner:?} was not borrow checked, MIR lowering does not support {what}")
            }
            Refused::Lowering { owner, error } => {
                write!(f, "the MIR of {owner:?} could not be built: {error}")
            }
            Refused::NotOf { owner, mir_owner } => {
                write!(f, "MIR of {mir_owner} given as that of {owner:?}")
            }
        }
    }
}

impl std::error::Error for Refused {}

/// What a borrow check result lets run: errors first, as they say the most.
pub(crate) fn verdict(owner: DefWithBodyId, result: &BorrowckResult) -> Result<(), Refused> {
    let errors: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|it| it.is_error())
        .cloned()
        .collect(); // alloc: a refusal's own errors; empty, so no allocation, for a passing body
    if !errors.is_empty() {
        return Err(Refused::Errors { owner, errors });
    }
    if let Some(why) = result.not_checked
        && why.refuses()
    {
        return Err(Refused::NotChecked { owner, why });
    }
    if let Some(what) = &result.skipped {
        let what = what.clone(); // clone: a refusal owns its text, on the error path
        return Err(Refused::Unsupported { owner, what });
    }
    Ok(())
}

/// The build path: `def`'s borrow check, which must pass before any of its code is compiled
/// for a build or taken from the store into one.
pub fn checked(db: &Db, def: DefWithBodyId) -> Result<Checked<'_>, Refused> {
    if !hir_ty::check_cache::served_passed(db, def) && !stored_passed(db, def) {
        verdict(def, borrowck(db, def))?;
    }
    Ok(Checked { owner: def, _revision: PhantomData })
}

/// Whether the item store holds a passing verdict of `def` by the checker in use, filed by
/// [`borrowck`] when one passed.
fn stored_passed(db: &Db, def: DefWithBodyId) -> bool {
    let Some(checker) = crate::checker(crate::mode()) else { return false };
    hir_ty::body_store::lookup_passed(db, def, checker, || verdict(def, borrowck(db, def)).is_ok())
}

fn mir_of<'db>(
    db: &'db Db,
    def: DefWithBodyId,
    body: Result<&'db MirBody<'db>, MirLowerError<'db>>,
) -> Result<&'db MirBody<'db>, Refused> {
    let body = body.map_err(|error| {
        let error = format!("{error:?}"); // alloc: a refusal's message, on the error path
        Refused::Lowering { owner: def, error }
    })?;
    // MIR of `def`'s body or of a closure, coroutine or `const` block in it, which its borrow
    // check covers.
    if body.owner.expression_store_owner(db) != ExpressionStoreOwnerId::Body(def) {
        let mir_owner = format!("{:?}", body.owner); // alloc: a refusal's message, error path
        return Err(Refused::NotOf { owner: def, mir_owner });
    }
    Ok(body)
}

impl<'db> Checked<'db> {
    pub fn owner(self) -> DefWithBodyId {
        self.owner
    }

    /// `body`, any MIR of the checked body or of a closure, coroutine or `const` block in it
    /// (built, runtime, optimized, monomorphized), for a build's codegen.
    pub fn mir(
        self,
        db: &'db Db,
        body: Result<&'db MirBody<'db>, MirLowerError<'db>>,
    ) -> Result<CheckedMir<'db>, Refused> {
        Ok(CheckedMir { owner: self, body: mir_of(db, self.owner, body)? })
    }
}

/// MIR whose borrow check passed: what a build's codegen compiles.
#[derive(Clone, Copy, Debug)]
pub struct CheckedMir<'db> {
    owner: Checked<'db>,
    body: &'db MirBody<'db>,
}

impl<'db> CheckedMir<'db> {
    pub fn checked(self) -> Checked<'db> {
        self.owner
    }

    pub fn body(self) -> &'db MirBody<'db> {
        self.body
    }
}

impl<'db> std::ops::Deref for CheckedMir<'db> {
    type Target = MirBody<'db>;

    fn deref(&self) -> &MirBody<'db> {
        self.body
    }
}

/// The live-patch path's entry point: `def`'s code may be compiled through the [`Unchecked`]
/// now, and the patch that runs it must be rolled back unless the [`Pending`] check finishes
/// without error. Never for a build's output.
pub fn speculate(def: DefWithBodyId) -> (Unchecked, Pending) {
    (Unchecked { owner: def }, Pending { owner: def })
}

/// A body whose code the live-patch path compiles before its borrow check finishes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Unchecked {
    owner: DefWithBodyId,
}

impl Unchecked {
    pub fn owner(self) -> DefWithBodyId {
        self.owner
    }

    /// `body`, any MIR of the body or of a closure, coroutine or `const` block in it, for the
    /// live patch's codegen.
    pub fn mir<'db>(
        self,
        db: &'db Db,
        body: Result<&'db MirBody<'db>, MirLowerError<'db>>,
    ) -> Result<UncheckedMir<'db>, Refused> {
        Ok(UncheckedMir { owner: self, body: mir_of(db, self.owner, body)? })
    }
}

/// MIR whose borrow check has not finished: what the live patch's codegen compiles.
#[derive(Clone, Copy, Debug)]
pub struct UncheckedMir<'db> {
    owner: Unchecked,
    body: &'db MirBody<'db>,
}

impl<'db> UncheckedMir<'db> {
    pub fn unchecked(self) -> Unchecked {
        self.owner
    }

    pub fn body(self) -> &'db MirBody<'db> {
        self.body
    }
}

impl<'db> std::ops::Deref for UncheckedMir<'db> {
    type Target = MirBody<'db>;

    fn deref(&self) -> &MirBody<'db> {
        self.body
    }
}

/// The borrow check a live patch waits on. It names no revision of the database, so it can be
/// sent to the task that finishes it.
#[derive(PartialEq, Eq, Debug)]
#[must_use = "a live patch is rolled back unless its borrow check finishes without error"]
pub struct Pending {
    owner: DefWithBodyId,
}

impl Pending {
    pub fn owner(&self) -> DefWithBodyId {
        self.owner
    }

    /// Runs the borrow check, or takes its result if it ran: `Err` means the patch must be
    /// rolled back.
    pub fn finish(self, db: &Db) -> Result<Checked<'_>, Refused> {
        checked(db, self.owner)
    }
}
