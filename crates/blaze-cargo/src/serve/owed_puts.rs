//! [`OwedPuts`]: a batch of store puts someone still owes the store. A worker never writes the
//! store; it hands its puts on to the loop, which does. A batch that is dropped on the way (an
//! early return, a `?`, a `let _ =`) is lost, and when a saved session already names the
//! entries in it, every later build that reads through that session fails ("its entry is not
//! in the store"). So a batch is a value that must be spent: [`OwedPuts::hand_on`] gives the
//! puts to whoever stores them, [`OwedPuts::discard`] drops them saying why that is right.
//!
//! The type is `#[blaze::linear]` (crates/hir-ty-linear, docs/v2/plan/design/linear.md): a
//! path that drops a batch any other way is a compile error, refused with the borrow check,
//! naming the batch, the exit that drops it (`return` or `?` at its line, a scope's end, an
//! arm that does not move it) and the two functions that spend it. It has no `Drop` impl on
//! purpose: a `Drop` would be its abandonment path, which lets an error exit (`?`, a returned
//! `Err`) drop a live batch silently, and a failed build loses puts a session names as surely
//! as a finished one. A panic is not checked; unwinding drops the puts with the batch.
#![blaze::sans_io]
#![expect(
    dead_code,
    reason = "made ahead of its callers: `Phases::body_puts`, `BodyPuts::puts` and `CarryOut::puts` move to it next"
)]
#![expect(
    blaze::unwired,
    reason = "unwired until `Phases::body_puts`, `BodyPuts::puts` and `CarryOut::puts` move onto it"
)]

use std::panic::Location;

use super::work::Put;

/// A batch of puts owed to the store, with where it was made. Spend it with
/// [`OwedPuts::hand_on`] or [`OwedPuts::discard`]; nothing else ends one.
///
/// The invariant: a batch, empty or not, leaves a body only through `hand_on`, `discard`, a
/// move into a function that takes it by value, or the body's return value; checked by the
/// front end on every path that does not unwind.
#[blaze::linear]
#[must_use = "store puts are owed: hand them on to the loop (`hand_on`) or `discard` them with a reason"]
#[derive(Debug)]
pub(crate) struct OwedPuts {
    puts: Vec<Put>,
    made: &'static Location<'static>,
}

impl OwedPuts {
    /// `puts`, owed from the caller's line on.
    #[track_caller]
    pub(crate) fn new(puts: Vec<Put>) -> OwedPuts {
        OwedPuts { puts, made: Location::caller() }
    }

    /// Where the batch was made.
    pub(crate) fn made(&self) -> &'static Location<'static> {
        self.made
    }

    pub(crate) fn len(&self) -> usize {
        self.puts.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.puts.is_empty()
    }

    /// The puts, read without spending the batch.
    pub(crate) fn iter(&self) -> std::slice::Iter<'_, Put> {
        self.puts.iter()
    }

    /// Takes `other`'s puts into this batch, which now owes them: `other` is paid.
    pub(crate) fn extend(&mut self, other: OwedPuts) {
        self.puts.extend(other.hand_on()); // alloc: the batch grows by the puts it takes, once a merge
    }

    /// Hands the puts on to the one who stores them: the batch is paid.
    #[blaze::consumes]
    pub(crate) fn hand_on(self) -> Vec<Put> {
        let OwedPuts { puts, made: _ } = self;
        puts
    }

    /// Drops the puts on purpose; `reason` says why nothing is lost by it ("the test drops a
    /// serve's puts, as a loop that never puts them would").
    #[blaze::consumes]
    pub(crate) fn discard(self, reason: &'static str) {
        let OwedPuts { puts, made } = self;
        if !puts.is_empty() {
            tracing::debug!(
                target: "blaze",
                puts = puts.len() as u64,
                made = %made,
                "store puts discarded: {reason}"
            );
        }
    }
}

impl Default for OwedPuts {
    /// An empty batch: owed all the same, spent like any other.
    #[track_caller]
    fn default() -> OwedPuts {
        OwedPuts::new(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::{OwedPuts, Put};

    fn puts(n: u8) -> Vec<Put> {
        (0..n)
            .map(|at| Put {
                key: blaze_cas::Key(blaze_ids::ContentId([at; 32])),
                kind: blaze_cas::Kind::Query,
                bytes: vec![at], // alloc: a test's put
            })
            .collect() // alloc: a test's batch
    }

    #[test]
    fn handed_on_gives_every_put_back() {
        let made = puts(3);
        let owed = OwedPuts::new(made.clone()); // clone: compared below
        assert_eq!(owed.len(), 3);
        assert_eq!(owed.made().file(), file!());
        assert_eq!(owed.hand_on(), made);
    }

    #[test]
    fn extended_owes_both_and_pays_the_other() {
        let mut owed = OwedPuts::new(puts(2));
        owed.extend(OwedPuts::new(puts(3)));
        assert_eq!(owed.hand_on().len(), 5);
    }

    #[test]
    fn discarded_with_a_reason_is_fine() {
        OwedPuts::new(puts(2)).discard("the test drops them on purpose");
        OwedPuts::default().discard("an empty batch, spent like any other");
    }
}
