//! The seam the engine implements ([`Source`]) and the one loop that drives the machine through
//! it ([`resolve`]). The machine asks for three things, each at its own grain:
//!
//! - ONE module's outline: blaze-file-items' `outline(text, edition, alloc)` of the module's file
//!   (or the inline `mod` body inside it), turned into [`Entry`]s: items, `use` lines, `extern
//!   crate`, and item-position calls with their [`Writes`](crate::Writes) summary.
//! - ONE call's expansion: blaze-expand's `expand(def, call, cx, alloc)` and the outline of its
//!   output (blaze-file-items' `outline_token_record`), as entries.
//! - ONE crate's root facts: the extern prelude, the prelude's path, the exporting modules.
//!
//! The loop gives every need of a step before the next step, so a driver may fetch a batch side
//! by side; it never answers a need the machine did not list.

use blaze_id::CrateItemKey;
use blaze_own::{AVec, Alloc, Vec};

use crate::resolver::{Resolver, Step};
use crate::types::{Answer, Ask, CallRef, CrateRoot, Entry, Error, Given, ModRef, Need, Outline};

/// Steps one ask may take: each answers at least one need, so this is a bound on needs too.
pub const MOST_STEPS: usize = 1 << 16;

/// Where the givens come from: the records and tasks of the engine (or a simulated world).
pub trait Source<'w> {
    /// Why the source could not give one (a record unreadable, an expansion refused).
    type Error;

    fn outline(&mut self, module: ModRef) -> Result<&'w Outline<'w>, Self::Error>;
    fn expansion(&mut self, module: ModRef, call: CallRef) -> Result<&'w [Entry<'w>], Self::Error>;
    fn root(&mut self, krate: CrateItemKey) -> Result<&'w CrateRoot<'w>, Self::Error>;
}

/// Why [`resolve`] stopped.
#[derive(Debug)]
pub enum DriveError<E> {
    Machine(Error),
    Source { need: Need, why: E },
    Bound { most: usize },
}

impl<E: core::fmt::Display> core::fmt::Display for DriveError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DriveError::Machine(why) => write!(f, "{why}"),
            DriveError::Source { need, why } => write!(f, "names on demand: the source could not give {need:?}: {why}"),
            DriveError::Bound { most } => write!(f, "names on demand: an ask past {most} steps (rule 18)"),
        }
    }
}

/// Answers `ask` with `resolver`, giving what it needs from `source`. `scratch` holds one step's
/// list of needs.
pub fn resolve<'w, S: Source<'w>>(
    resolver: &mut Resolver<'_, 'w>,
    source: &mut S,
    ask: Ask<'w>,
    scratch: Alloc<'_>,
) -> Result<Answer, DriveError<S::Error>> {
    resolver.ask(ask).map_err(DriveError::Machine)?;
    let mut wanted: AVec<'_, Need> = Vec::new_in(scratch);
    for _ in 0..MOST_STEPS {
        match resolver.step().map_err(DriveError::Machine)? {
            Step::Answered(answer) => return Ok(answer),
            Step::Needs => {
                wanted.clear();
                wanted.try_extend_from_slice(resolver.needs()).map_err(|why| DriveError::Machine(Error::Memory(why)))?;
                for need in wanted.iter().copied() {
                    let given = match need {
                        Need::Outline(m) => Given::Outline(source.outline(m).map_err(|why| DriveError::Source { need, why })?),
                        Need::Expand { module, call } => Given::Expansion {
                            module,
                            call,
                            entries: source.expansion(module, call).map_err(|why| DriveError::Source { need, why })?,
                        },
                        Need::Root(krate) => Given::Root(source.root(krate).map_err(|why| DriveError::Source { need, why })?),
                    };
                    resolver.give(given).map_err(DriveError::Machine)?;
                }
            }
        }
    }
    Err(DriveError::Bound { most: MOST_STEPS })
}
