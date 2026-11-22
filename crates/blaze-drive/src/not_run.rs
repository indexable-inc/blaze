//! What did not run, grouped by the failure that stopped it (drive.md, D115 ruling (6)).
//!
//! A task that failed `Why::Waited { on }` names only the need that stopped it directly. The
//! answer says what was not checked by CAUSE: each waited task is put under the first task down
//! its `on` chain that failed for a reason of its own (not `Waited`). A worker answers `Waited`
//! for any need that settled with no record, which includes a name DROPPED this run (its home
//! re-made without it: no failure entry); such an `on` is a cause of its own, `Cause::Dropped`.
//!
//! The caller names each cause by its item and maps each waited task to its crate (blaze-cli,
//! main.rs's failure printing); this file only groups, sans IO.

use blaze_own::{AVec, Alloc, Vec};

use crate::key::Name;
use crate::task::{TaskFailure, Why};

/// The task a group of waited tasks comes down to.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Cause {
    /// It failed this command for a reason of its own (its failure is in the list).
    Failed(Name),
    /// It was needed but dropped this run: no task of that name exists any more.
    Dropped(Name),
}

/// The tasks that did not run because of one cause, sorted by name (so by kind first).
#[derive(Debug)]
pub struct NotRun<'a> {
    pub cause: Cause,
    pub waited: AVec<'a, Name>,
}

/// The failures cannot be grouped: a chain of `Waited` that comes back to itself. A drive
/// never makes one (a task waits only on a need that settled before it); it is refused by
/// name, never broken at a guess (rule 3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NotRunError {
    Cycle { at: Name },
}

impl core::fmt::Display for NotRunError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            NotRunError::Cycle { at } => {
                write!(f, "the failed tasks wait on each other in a ring through {at}: no cause can be named (not_run.rs, rule 3)")
            }
        }
    }
}

/// The waited tasks of `failures`, grouped by cause: groups sorted by cause, each group's tasks
/// sorted and without repeats. A cause with no waited task makes no group. `O(n log n + n d)`
/// for `n` failures and chains `d` long.
pub fn by_cause<'a>(failures: &[TaskFailure], alloc: Alloc<'a>) -> Result<AVec<'a, NotRun<'a>>, NotRunError> {
    // (name, what it waited on), sorted by name: `None` for a failure of its own.
    let mut on: AVec<'a, (Name, Option<Name>)> = Vec::with_capacity_in(failures.len(), alloc);
    for failure in failures {
        let waited = match failure.why {
            Why::Waited { on } => Some(on),
            _ => None,
        };
        on.push((failure.name, waited));
    }
    on.sort_unstable_by_key(|(name, _)| *name);
    // (cause, waited task), one a waited failure.
    let mut pairs: AVec<'a, (Cause, Name)> = Vec::new_in(alloc);
    for &(name, first) in on.iter() {
        let Some(mut at) = first else { continue };
        let mut steps = 0usize;
        let cause = loop {
            match on.binary_search_by_key(&at, |(name, _)| *name) {
                Err(_) => break Cause::Dropped(at),
                Ok(i) => match on[i].1 {
                    None => break Cause::Failed(at),
                    Some(next) => {
                        steps += 1;
                        // A chain longer than the list repeats a name: a ring.
                        if steps > on.len() {
                            return Err(NotRunError::Cycle { at: name });
                        }
                        at = next;
                    }
                },
            }
        };
        pairs.push((cause, name));
    }
    pairs.sort_unstable();
    pairs.dedup();
    let mut groups: AVec<'a, NotRun<'a>> = Vec::new_in(alloc);
    let mut i = 0;
    while i < pairs.len() {
        let cause = pairs[i].0;
        let n = pairs[i..].iter().take_while(|(c, _)| *c == cause).count();
        let mut waited = Vec::with_capacity_in(n, alloc);
        for (_, name) in pairs[i..i + n].iter() {
            waited.push(*name);
        }
        groups.push(NotRun { cause, waited });
        i += n;
    }
    Ok(groups)
}

#[cfg(test)]
#[path = "not_run_tests.rs"]
mod tests;
