//! The machine's own failures: a record it cannot read, a bound exceeded, a protocol broken by its
//! caller. User code's errors are not these; they are [`crate::ScopeError`] values in a scope.

use core::fmt;

use blaze_id::ItemKey;
use blaze_surface::SurfaceError;

/// Why a machine stopped.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ResolveError {
    /// A record the resolution of `module` reads could not be read: missing, not built yet,
    /// or of other bytes than its schema (the error names which record, and of what).
    Surface { module: ItemKey, error: SurfaceError },
    /// A module's chain of parents is longer than [`crate::MAX_MODULE_DEPTH`] (a cycle in the
    /// module facts, or a tree no real crate has).
    ModuleTooDeep { module: ItemKey },
    /// More expansions in one module than [`crate::MAX_EXPANSIONS`].
    TooManyExpansions { module: ItemKey },
    /// A run of passes that kept changing state past the bound the work allows: a bug.
    NoProgressBound { module: ItemKey, passes: u32 },
    /// A group's glob fixpoint of one name kept changing past its bound: a bug.
    GlobFixpointBound { module: ItemKey },
    /// The final pass found a name of `module` still undetermined with every answer in:
    /// `glob_of` names the module whose glob left it so (none: no glob did), `unexpanded`
    /// the calls of `module` that may still add names. A bug.
    Undecided { module: ItemKey, glob_of: Option<ItemKey>, unexpanded: u32 },
    /// A member's final pass waited on something with every answer in: a bug.
    MemberUnfinished { module: ItemKey },
    /// An answer of the wrong kind for its read.
    WrongAnswer { module: ItemKey, read_of: ItemKey },
    /// A step after the machine finished.
    Finished { module: ItemKey },
    /// A group made of no machine, or of two machines of one module.
    BadGroup { module: Option<ItemKey> },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ResolveError::Surface { module, error } => {
                write!(f, "name resolution of module {module:x}: {error}")
            }
            ResolveError::ModuleTooDeep { module } => write!(
                f,
                "name resolution of module {module:x}: more than {} parents",
                crate::MAX_MODULE_DEPTH
            ),
            ResolveError::TooManyExpansions { module } => write!(
                f,
                "name resolution of module {module:x}: more than {} macro expansions",
                crate::MAX_EXPANSIONS
            ),
            ResolveError::NoProgressBound { module, passes } => write!(
                f,
                "name resolution of module {module:x}: {passes} passes changed state, past the bound its work allows"
            ),
            ResolveError::GlobFixpointBound { module } => write!(
                f,
                "name resolution of the group of module {module:x}: a glob fixpoint changed past its bound"
            ),
            ResolveError::Undecided { module, glob_of, unexpanded } => {
                write!(f, "name resolution of module {module:x}: a name undetermined in the final pass (")?;
                match glob_of {
                    Some(m) => write!(f, "by a glob of {m:x}")?,
                    None => write!(f, "by no glob")?,
                }
                write!(f, ", {unexpanded} calls unexpanded)")
            }
            ResolveError::MemberUnfinished { module } => {
                write!(f, "name resolution of module {module:x}: the final pass waited with every answer in")
            }
            ResolveError::WrongAnswer { module, read_of } => write!(
                f,
                "name resolution of module {module:x}: a read of {read_of:x} was answered with the wrong kind of answer"
            ),
            ResolveError::Finished { module } => {
                write!(f, "name resolution of module {module:x}: stepped after it finished")
            }
            ResolveError::BadGroup { module: Some(module) } => {
                write!(f, "a resolution group holds module {module:x} twice")
            }
            ResolveError::BadGroup { module: None } => write!(f, "a resolution group of no module"),
        }
    }
}

impl core::error::Error for ResolveError {}
