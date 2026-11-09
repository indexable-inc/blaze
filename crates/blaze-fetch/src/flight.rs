//! One download per pin at a time into a store: the first request of a pin leads, and every
//! other request of it while the leader works follows, waiting for the leader to end and then
//! reading the store. A flight is keyed by the store and the origin's record key (the pin and
//! the form asked for, [`crate::Origin::key`]): two stores of one process (a test's, a
//! fetcher given its own) never wait on each other, and a follower always reads the store its
//! leader filed into.
//!
//! A follower's wait is a receive on a channel the leader's end disconnects, bounded by a
//! deadline and in a span naming the pin (AGENTS.md rules 15 and 18). Nothing is handed from
//! leader to follower but that the leader ended: the follower reads the store, so it is only
//! ever given bytes the store verified.

use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};

use crate::digest::Digest;

/// What a flight is keyed by: the store's address in the process
/// ([`crate::Objects`]'s, compared only in the process) and the origin's record key.
pub type Flight = (usize, blaze_cas::Key);

/// The pins being downloaded, each with the receiver its followers wait on.
pub struct Flights {
    live: papaya::HashMap<Flight, Receiver<()>>,
    /// Told once each time a request begins to follow: a test's proof that its requests
    /// waited on a leader rather than downloading or finding the store already filled.
    followed: Option<Sender<()>>,
}

/// What a request of a pin is to do.
pub enum Role<'f> {
    /// Download it; followers wait until this drops.
    Lead(Leader<'f>),
    /// Wait for the leader ([`Follower::wait`]), then read the store.
    Follow(Follower),
}

pub struct Leader<'f> {
    flights: &'f Flights,
    flight: Flight,
    /// Dropped after the pin leaves the table ([`Drop`] runs before fields drop), which wakes
    /// every follower.
    _ended: Sender<()>,
}

impl Drop for Leader<'_> {
    fn drop(&mut self) {
        self.flights.live.pin().remove(&self.flight);
    }
}

pub struct Follower {
    pin: Digest,
    ended: Receiver<()>,
}

/// How a follower's wait ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Waited {
    /// The leader ended, well or not: the store says which.
    LeaderEnded,
    /// The deadline came first.
    TimedOut { waited: Duration },
}

impl Default for Flights {
    fn default() -> Flights {
        Flights::new()
    }
}

impl Flights {
    pub fn new() -> Flights {
        Flights { live: papaya::HashMap::new(), followed: None }
    }

    /// A table that sends on `followed` each time a request begins to follow (a test's).
    pub fn watched(followed: Sender<()>) -> Flights {
        Flights { live: papaya::HashMap::new(), followed: Some(followed) }
    }

    /// The process's table: every [`crate::Fetcher`] of the process shares it.
    pub fn process() -> &'static Flights {
        #[expect(
            clippy::disallowed_types,
            reason = "the process's one single-flight table: every fetcher into the process's one store shares it"
        )]
        static FLIGHTS: std::sync::OnceLock<Flights> = std::sync::OnceLock::new();
        FLIGHTS.get_or_init(Flights::new)
    }

    /// Leads `flight`, or follows its leader; `pin` names it in a follower's span.
    pub fn begin(&self, flight: Flight, pin: Digest) -> Role<'_> {
        let (ended, waiting) = crossbeam_channel::bounded(0);
        match self.live.pin().try_insert(flight, waiting) {
            Ok(_) => Role::Lead(Leader { flights: self, flight, _ended: ended }),
            Err(occupied) => {
                if let Some(followed) = &self.followed {
                    _ = followed.send(());
                }
                // clone: a channel's receiver, once a follower
                Role::Follow(Follower { pin, ended: occupied.current.clone() })
            }
        }
    }

    /// How many pins are being downloaded.
    pub fn len(&self) -> usize {
        self.live.pin().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Follower {
    /// Waits until the leader ends or `most` passes.
    pub fn wait(&self, most: Duration) -> Waited {
        let _span = tracing::info_span!(target: "blaze", "blaze-fetch waits for another request's download", pin = %self.pin).entered();
        let start = Instant::now();
        match self.ended.recv_timeout(most) {
            // Nothing is ever sent: the only end is the leader's sender dropping.
            Ok(()) | Err(RecvTimeoutError::Disconnected) => Waited::LeaderEnded,
            Err(RecvTimeoutError::Timeout) => Waited::TimedOut { waited: start.elapsed() },
        }
    }
}
