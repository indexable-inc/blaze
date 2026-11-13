//! [`OwedPuts`]: a batch of store puts someone still owes the store. A worker never writes the
//! store; it hands its puts on to the loop, which does. A batch that is dropped on the way (an
//! early return, a `?`, a `let _ =`) is lost, and when a saved session already names the
//! entries in it, every later build that reads through that session fails. So a batch is a
//! value that must be spent: [`OwedPuts::hand_on`] gives the puts to whoever stores them,
//! [`OwedPuts::discard`] drops them saying why that is right. Dropped any other way, it is a
//! bug named by where the batch was made: a panic in a debug build, and in a release build an
//! error logged and counted ([`dropped`]).
//!
//! Not `#[blaze::sans_io]`: its `Drop` asks `std::thread::panicking()`, which the sans-IO lint
//! counts as a thread effect.
#![expect(
    dead_code,
    reason = "made ahead of its callers: `Phases::body_puts`, `BodyPuts::puts` and `CarryOut::puts` move to it next"
)]
#![expect(
    blaze::unwired,
    reason = "unwired until `Phases::body_puts`, `BodyPuts::puts` and `CarryOut::puts` move onto it"
)]

use std::{
    panic::Location,
    sync::atomic::{AtomicU64, Ordering},
};

use super::work::Put;

/// Batches of puts dropped without being handed on or discarded since the process started.
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// How many batches of puts were dropped while still owed, since the process started: zero in
/// a correct blaze.
pub(crate) fn dropped() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

/// A batch of puts owed to the store, with where it was made. `#[must_use]`, and checked on
/// drop: spend it with [`OwedPuts::hand_on`] or [`OwedPuts::discard`].
///
/// The invariant: a non-empty batch leaves only through `hand_on` or `discard`; any other drop
/// is counted in [`dropped`], and panics in a debug build unless a panic is already unwinding.
/// An empty batch owes nothing and drops freely.
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
    pub(crate) fn hand_on(mut self) -> Vec<Put> {
        std::mem::take(&mut self.puts)
    }

    /// Drops the puts on purpose; `reason` says why nothing is lost by it ("the test drops a
    /// serve's puts, as a loop that never puts them would").
    pub(crate) fn discard(mut self, reason: &'static str) {
        if !self.puts.is_empty() {
            tracing::debug!(
                target: "blaze",
                puts = self.puts.len() as u64,
                made = %self.made,
                "store puts discarded: {reason}"
            );
        }
        self.puts.clear();
    }
}

impl Default for OwedPuts {
    /// An empty batch: owes nothing.
    #[track_caller]
    fn default() -> OwedPuts {
        OwedPuts::new(Vec::new())
    }
}

impl Drop for OwedPuts {
    fn drop(&mut self) {
        if self.puts.is_empty() {
            return;
        }
        // Never a second panic while one unwinds: that aborts the process and hides the first.
        lost(self.made, self.puts.len(), cfg!(debug_assertions) && !std::thread::panicking());
    }
}

/// A batch of `puts` puts made at `made` was dropped while owed: counted, then a panic when
/// `panic` (a debug build, not unwinding), else an error logged.
fn lost(made: &'static Location<'static>, puts: usize, panic: bool) {
    DROPPED.fetch_add(1, Ordering::Relaxed);
    if panic {
        panic!(
            "{puts} store puts made at {made} were dropped without being handed on or discarded \
             (`OwedPuts`): the store never gets them, while what named them may be kept"
        );
    }
    tracing::error!(
        target: "blaze",
        puts = puts as u64,
        made = %made,
        "store puts made at {made} were dropped without being handed on or discarded (`OwedPuts`): \
         the store never gets them, while what named them may be kept"
    );
}

#[cfg(test)]
mod tests {
    use super::{OwedPuts, Put, dropped, lost};

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
    }

    #[test]
    fn an_empty_batch_drops_freely() {
        drop(OwedPuts::default());
        drop(OwedPuts::new(Vec::new()));
    }

    /// The invariant: a batch dropped while owed panics in a debug build, naming the line that
    /// made it. The control is [`handed_on_gives_every_put_back`]: the same batch, spent.
    #[cfg(debug_assertions)]
    #[test]
    fn dropped_bare_panics_naming_where_it_was_made() {
        let mut made = None;
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let owed = OwedPuts::new(puts(1));
            made = Some(owed.made());
            drop(owed);
        }));
        let made = made.expect("the batch was made before the drop");
        assert_eq!(made.file(), file!());
        let payload = caught.expect_err("a batch dropped while owed panics in a debug build");
        let message = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .expect("the panic carries a message");
        let at = format!("{}:{}:{}", made.file(), made.line(), made.column()); // alloc: a test's expected text
        assert!(message.contains(&at), "the panic names {at}: {message}");
    }

    /// The release path, through the function the `Drop` calls, without the panic: counted.
    #[test]
    fn dropped_in_release_is_counted() {
        let before = dropped();
        lost(std::panic::Location::caller(), 4, false);
        assert!(dropped() > before, "a lost batch is counted");
    }
}
