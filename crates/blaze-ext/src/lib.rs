//! The API of blaze's extensions (docs/v2/plan/design/extensions.md): what a user's analysis
//! crate is compiled against.
//!
//! An extension is an ordinary `#![no_std]` crate whose only dependency is this one. Its crate
//! root has `pub fn extension() -> Extension`, which names its lints ([`Lint`]) and its facts
//! ([`FactDecl`]). blaze compiles the crate itself, natively, against the source this blaze was
//! built from, and calls it in process: a lint is given a [`Cx`] (every capability it has: its
//! fuel, its arena, the diagnostics sink, the facts it declared it asks for) and a [`Body`]
//! (blaze's real MIR, [`mir::MirBody`], with the few answers of the front end a body needs
//! beside it).
//!
//! The surface is chosen, not grown: everything an extension can name is re-exported here, and
//! a type re-exported from the front end brings its public fields and inherent methods with
//! it, so only plain data is re-exported and the database never is. [`VERSION`] names the
//! surface; [`CHANGES`] says what each version changed, so a refused extension is told what to
//! edit.
//!
//! What an extension cannot do is decided where it is compiled (`blaze-ext-host`'s checks), not
//! here: no `unsafe`, no `extern`, no statics, no `std`. What this crate adds is that nothing
//! of the host is forgeable from safe code: a [`host::Job`] is made only with a
//! [`host::HostKey`], which takes the database no extension is ever handed.

#![feature(allocator_api)]

mod body;
mod cx;
mod fact;
pub mod host;

pub use blaze_api::Applicability;
pub use body::{Body, Callee, ImplOf, Item};
pub use cx::{Arena, Bytes, Cx, Report, Vec};
pub use fact::{Analysis, Fact, FactDecl, FactFn};

/// blaze's MIR, as the front end builds it: the types of `hir_ty_mir::mir` an extension reads.
/// A body is [`mir::MirBody`]; its blocks are `body.basic_blocks`, each a
/// [`mir::BasicBlock`] of [`mir::Statement`]s and a [`mir::Terminator`].
pub mod mir {
    pub use hir_ty_mir::mir::{
        AggregateKind, AssertKind, BasicBlock, BasicBlockId, BinOp, BorrowKind, CastKind, Local,
        LocalId, MirBody, MirPhase, MirSpan, MutBorrowKind, Operand, OperandKind, Place,
        ProjectionElem, Rvalue, Statement, StatementKind, SwitchTargets, Terminator,
        TerminatorKind, UnOp,
    };
}

/// The version of this surface. An extension's manifest states the one its source was written
/// for (its `blaze-ext` requirement) and blaze refuses another by name; the surface is
/// unstable across blaze's versions by design, so the number moves whenever an item an
/// extension can name changes meaning or shape.
pub const VERSION: &str = "0.1.0";

/// What each version of the surface changed, oldest first: the items an extension written for
/// the version before has to look at. A refusal of a version mismatch quotes the entries
/// after the extension's version.
pub const CHANGES: &[(&str, &[&str])] = &[(
    "0.1.0",
    &["the first surface: `Extension`, `Lint`, `FactDecl`, `Cx`, `Body`, `Callee`, `mir::*`"],
)];

/// What an extension's crate root gives blaze: `pub fn extension() -> Extension`.
#[derive(Clone, Copy)]
pub struct Extension {
    /// As the workspace's manifest names it; every diagnostic and error of it says so.
    pub name: &'static str,
    /// The [`VERSION`] the crate was compiled against: `blaze_ext::VERSION`.
    pub api: &'static str,
    pub lints: &'static [Lint],
    pub facts: &'static [FactDecl],
}

/// One lint: a function of a body that reports what it finds through its [`Cx`].
#[derive(Clone, Copy)]
pub struct Lint {
    /// Its name after the extension's: `panic_in_drop` of `panic-in-drop` is reported as
    /// `panic_in_drop::panic_in_drop`.
    pub name: &'static str,
    /// Its level where the workspace's manifest sets none.
    pub level: Level,
    /// One line: what it finds.
    pub summary: &'static str,
    /// The facts it asks for, by name, of the body's own item and of its callees
    /// ([`Cx::fact`]); asking for another is a stop naming it.
    pub requires: &'static [&'static str],
    pub run: LintFn,
}

/// A lint's function: called once per body of the crate being checked.
pub type LintFn = for<'a, 'b, 'db> fn(&mut Cx<'a>, &Body<'b, 'db>);

/// The level a lint is reported at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Level {
    /// Not run.
    Allow,
    Warn,
    /// An error: the check fails.
    Deny,
}

impl Level {
    /// As a manifest writes it.
    pub const fn name(self) -> &'static str {
        match self {
            Level::Allow => "allow",
            Level::Warn => "warn",
            Level::Deny => "deny",
        }
    }

    /// The level `name` is, as a manifest writes it.
    pub fn named(name: &str) -> Option<Level> {
        [Level::Allow, Level::Warn, Level::Deny].into_iter().find(|it| it.name() == name)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_surface_s_version_is_the_crate_s() {
        assert_eq!(super::VERSION, env!("CARGO_PKG_VERSION"));
        assert_eq!(super::CHANGES.last().map(|it| it.0), Some(super::VERSION));
    }
}
