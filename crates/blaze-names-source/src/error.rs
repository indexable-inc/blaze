//! Why the source could not give the demand machine what it asked (rule 19: each names the
//! module, the call, the crate or the file it is about).

use core::fmt;

use blaze_id::{CrateItemKey, ItemKey};
use blaze_names_demand::types::{CallRef, Error as MachineError, ModRef};
use blaze_own::AllocFailure;

/// Why [`crate::CrateSource`] stopped. Every variant names what it is about; none is a fallback.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SourceError<'w> {
    Memory(AllocFailure),
    /// A crate key the source was not given ([`crate::CrateFacts`]).
    NoCrate(CrateItemKey),
    /// A crate's root file its texts do not hold.
    NoFile { krate: CrateItemKey, path: &'w str },
    /// A module no outline this source made names: a key the machine invented.
    UnknownModule(ModRef),
    /// `mod name { .. }` or `mod name;` written by a macro expansion: a module the walk did not
    /// see, whose outline (and, for `mod name;`, file by the expanding module's directory) this
    /// source does not make yet (blaze-scope `make_modules` does).
    MadeModule { module: ModRef, call: CallRef },
    /// An attribute or derive call asked to expand: only function-like calls are expanded here.
    Shape { module: ModRef, call: CallRef },
    /// A table at its cap (rule 18): `table` names it.
    Full { table: &'static str, most: usize },
    /// `pub(in path)` or `pub(path)`: a visibility by path, not read yet.
    VisPath { module: ModRef },
    /// `pub(super)` in a crate's root.
    SuperPastRoot { module: ModRef },
    /// `extern crate name;` of a crate the graph does not give this crate.
    ExternCrate { module: ModRef, name: &'w str },
    /// `use ::name;` (a whole crate imported by `::`): no module path to import from.
    UseRoot { module: ModRef },
    /// An expansion asked for a call the source never outlined in that module.
    UnknownCall { module: ModRef, call: CallRef },
    /// A call whose macro the macro namespace does not name.
    NoMacro { module: ModRef, call: CallRef },
    /// A macro item whose definition blaze-scope's `definition_of` refused, or one no outline
    /// lowered.
    Definition { item: ItemKey },
    /// A call's input that does not lex.
    Lex { module: ModRef, call: CallRef },
    /// blaze-expand refused the call.
    Expand { module: ModRef, call: CallRef },
    /// An eager macro, whose nested calls must be expanded first: not taken in item position
    /// here yet.
    Eager { module: ModRef, call: CallRef },
    /// The expansion carries an error diagnostic (`compile_error!`, no arm matched, ..).
    Failed { module: ModRef, call: CallRef },
    /// The expansion holds a `$crate` token: rendered as text it would lose its crate
    /// (blaze-scope's `render_with` keeps it as a mark; not shared yet).
    DollarCrate { module: ModRef, call: CallRef },
    /// Finding a callee asked through more nested resolvers than [`crate::MOST_CALLEE_DEPTH`]
    /// (rule 18): a macro's summary that needs a macro whose summary needs a macro ..
    CalleeDepth { module: ModRef },
    /// A callee found while the calling module was summarised (its own calls taken as writing
    /// nothing) is another once the module's summaries are in (rustc's time-travel ambiguity of a
    /// macro-expanded macro, E0659).
    CalleeMoved { module: ModRef, call: CallRef },
    /// The demand machine refused (a nested resolver's).
    Machine(MachineError),
    /// A nested resolver's ask past blaze-names-demand's step bound.
    Steps { module: ModRef },
    /// This crate's own invariant broken: a bug here.
    Internal(&'static str),
}

impl From<AllocFailure> for SourceError<'_> {
    fn from(why: AllocFailure) -> Self {
        SourceError::Memory(why)
    }
}

impl fmt::Display for SourceError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceError::Memory(why) => write!(f, "names source: {why}"),
            SourceError::NoCrate(krate) => write!(f, "names source: crate {krate:?} was not given"),
            SourceError::NoFile { krate, path } => write!(f, "names source: crate {krate:?} has no file `{path}`"),
            SourceError::UnknownModule(m) => write!(f, "names source: module {m:?} is in no outline made"),
            SourceError::MadeModule { module, call } => {
                write!(f, "names source: call {call:?} in {module:?} writes a module, not outlined here yet")
            }
            SourceError::Shape { module, call } => {
                write!(f, "names source: call {call:?} in {module:?} is an attribute or derive, not expanded here yet")
            }
            SourceError::Full { table, most } => write!(f, "names source: the {table} table is at its cap of {most} (rule 18)"),
            SourceError::VisPath { module } => write!(f, "names source: `pub(in path)` in {module:?} is not read yet"),
            SourceError::SuperPastRoot { module } => write!(f, "names source: `pub(super)` in the crate root {module:?}"),
            SourceError::ExternCrate { module, name } => {
                write!(f, "names source: `extern crate {name}` in {module:?} names no crate of the graph")
            }
            SourceError::UseRoot { module } => write!(f, "names source: `use ::crate_name;` in {module:?} is not read yet"),
            SourceError::UnknownCall { module, call } => write!(f, "names source: no call {call:?} was outlined in {module:?}"),
            SourceError::NoMacro { module, call } => write!(f, "names source: the macro of call {call:?} in {module:?} is not found"),
            SourceError::Definition { item } => write!(f, "names source: the macro {item:?} has no definition blaze-expand takes"),
            SourceError::Lex { module, call } => write!(f, "names source: the input of call {call:?} in {module:?} does not lex"),
            SourceError::Expand { module, call } => write!(f, "names source: blaze-expand refused call {call:?} in {module:?}"),
            SourceError::Eager { module, call } => write!(f, "names source: call {call:?} in {module:?} is eager"),
            SourceError::Failed { module, call } => write!(f, "names source: call {call:?} in {module:?} expanded to an error"),
            SourceError::DollarCrate { module, call } => {
                write!(f, "names source: the expansion of {call:?} in {module:?} holds `$crate`, not rendered by mark yet")
            }
            SourceError::CalleeDepth { module } => {
                write!(f, "names source: a callee asked from {module:?} past {} nested resolvers (rule 18)", crate::MOST_CALLEE_DEPTH)
            }
            SourceError::CalleeMoved { module, call } => write!(
                f,
                "names source: the macro of call {call:?} in {module:?} is another once the module's own calls are summarised (E0659)"
            ),
            SourceError::Machine(why) => write!(f, "{why}"),
            SourceError::Steps { module } => write!(f, "names source: a callee asked from {module:?} past the step bound (rule 18)"),
            SourceError::Internal(what) => write!(f, "names source: internal: {what}"),
        }
    }
}

impl core::error::Error for SourceError<'_> {}
