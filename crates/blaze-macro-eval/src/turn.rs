//! The turn of a proc-macro session: the one call at a time that runs from a session's state
//! and changes it ([`expander`](crate::expander)'s `Session`). A call takes the turn before it
//! reads the state it runs from and gives it back once it has left the state its run made.
//!
//! A turn is a scheduler hold ([`Flights::hold_named`]), never an OS lock: the call holding it
//! runs the macro, which waits (its wasm expansion thread, a bridge call's query, a query
//! another job runs) with its fiber switched out, and a call that wants the turn meanwhile
//! waits the same way, holding no thread. A lock held across those waits deadlocked the
//! daemon: every worker blocked on the session's lock, and the holder, ready to go on, had
//! no thread left to run on (lane macrosession, 2026-11-03). Each wait for a turn opens a span
//! naming the session (the macro crate and the calling crate) and the job holding it
//! (every-wait-says-what), and one that would close a cycle of waits gives way, as a query's
//! wait does.

use blaze_graph::{
    Key,
    pump::{Flights, Held, NotHeld},
};
use blaze_query::db::Cancelled;

/// How many other calls' turns one call waits out before it fails (bounded-progress): each
/// ends a turn, so a call still waiting after this many is starved, and says so.
const MOST_TURNS_WAITED: usize = 1 << 16;

/// The turns of a library's sessions, each under its session's [`Key`].
pub(crate) struct Turns {
    turns: Flights<()>,
}

impl Default for Turns {
    fn default() -> Turns {
        Turns { turns: Flights::new() }
    }
}

/// Why a call has no turn.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum NoTurn {
    /// The job asking holds the turn already: a call of the session was asked for inside the
    /// run of another, which would wait on itself.
    HeldByThisJob,
    /// It waited out this many other calls' turns and was never given its own.
    Starved(usize),
}

impl NoTurn {
    /// Why, for an error that names the session (`session`) as the caller says it.
    pub(crate) fn said(&self, session: &str) -> String {
        match self {
            // alloc: error path
            NoTurn::HeldByThisJob => format!(
                "{session} was asked for a call while a call of it runs on the same job: the \
                 second call would wait for the first, which waits for it"
            ),
            // alloc: error path
            NoTurn::Starved(turns) => {
                format!(
                    "{session}: a call waited out {turns} turns of other calls and was never given its own"
                )
            }
        }
    }
}

/// A session's turn, held until dropped.
pub(crate) struct Turn<'t> {
    held: Option<Held<'t, ()>>,
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        if let Some(held) = self.held.take() {
            held.end(&());
        }
    }
}

impl Turns {
    /// The turn of the session `session`, which `named` names (the macro crate and the calling
    /// crate): taken at once when no call holds it, else waited for, the fiber switched out
    /// (holding no thread) until the holder gives it back.
    ///
    /// A wait that would close a cycle of waits gives way: it unwinds with
    /// [`Cancelled::WouldWaitOnItself`], and the loop running the query this call is part of
    /// runs it again, as for a query's own give-way.
    // dyn: what names the session, called only to say a wait or a hold: one shape for every caller
    pub(crate) fn take(
        &self,
        session: Key,
        named: &mut dyn FnMut(&mut String),
    ) -> Result<Turn<'_>, NoTurn> {
        for _ in 0..MOST_TURNS_WAITED {
            match self.turns.hold_named(session, &mut *named) {
                Ok(held) => return Ok(Turn { held: Some(held) }),
                Err(NotHeld::ByThisTask) => return Err(NoTurn::HeldByThisJob),
                Err(NotHeld::Busy) => {
                    // Waits for the holder's turn to end: a run of nothing, claimed here only
                    // when the turn was given back before this asked.
                    if self.turns.get_or_run_named(session, &mut *named, || Ok(())).is_err() {
                        Cancelled::WouldWaitOnItself { asked: None }.throw();
                    }
                }
            }
        }
        Err(NoTurn::Starved(MOST_TURNS_WAITED))
    }
}

#[cfg(test)]
mod tests;
