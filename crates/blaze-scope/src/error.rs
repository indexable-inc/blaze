//! The machine's own failures: a missing record, a bound exceeded, a protocol broken by its
//! caller. User code's errors are not these; they are [`crate::ScopeError`] values in a scope.

use core::fmt;

use blaze_id::{CrateItemKey, ItemKey};

/// Which record was missing.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Record {
    ModuleInfo,
    ModuleDecls,
    CrateRoot,
}

/// Why a machine stopped.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ResolveError {
    /// The facts have no such record for `key` (a module's, or a crate's key's first 8 bytes'
    /// owner for `CrateRoot`).
    Missing { record: Record, module: ItemKey },
    /// The facts have no crate-root record of `krate`.
    NoCrateRoot { module: ItemKey, krate: CrateItemKey },
    /// A module's chain of parents is longer than [`crate::MAX_MODULE_DEPTH`] (a cycle in the
    /// module facts, or a tree no real crate has).
    ModuleTooDeep { module: ItemKey },
    /// More expansions in one module than [`crate::MAX_EXPANSIONS`].
    TooManyExpansions { module: ItemKey },
    /// A run of passes that kept changing state past the bound the work allows: a bug.
    NoProgressBound { module: ItemKey, passes: u32 },
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
            ResolveError::Missing { record, module } => {
                write!(f, "name resolution of module {module:x}: no {record:?} record")
            }
            ResolveError::NoCrateRoot { module, krate } => write!(
                f,
                "name resolution of module {module:x}: no crate-root record of crate {krate:x}"
            ),
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
