//! The decisions of one fetch, as a synchronous state machine on an injected clock (AGENTS.md
//! rule 14): whether to ask again after a failure and when, when to give up, and whether what
//! came is what was pinned. It owns no socket, file, thread or timer; the driver
//! ([`crate::fetcher`]) does the IO, tells it the time ([`Attempts::advance_clock`]) and what
//! happened ([`Attempts::failed`]), and does what it decides ([`Attempts::next_decision`],
//! [`Attempts::next_deadline`]). A test replays a transcript with no real clock.

#![blaze::sans_io]

use std::time::Duration;

use crate::digest::{Digest, Hashed};

/// How often a download is asked for, and how long between: `tries` in all, the `n`th wait
/// `first_wait * 2^(n-1)`, never more than `most_wait`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Policy {
    pub tries: u32,
    pub first_wait: Duration,
    pub most_wait: Duration,
}

impl Default for Policy {
    /// Four tries, half a second, then one, then two: cargo's `net.retry` of 3 in count.
    fn default() -> Policy {
        Policy {
            tries: 4,
            first_wait: Duration::from_millis(500),
            most_wait: Duration::from_secs(10),
        }
    }
}

impl Policy {
    /// The wait after the `tried`th failure (from 1).
    fn wait(&self, tried: u32) -> Duration {
        let doubled = self.first_wait.saturating_mul(1u32 << (tried - 1).min(16));
        doubled.min(self.most_wait)
    }
}

/// Why one try got no bytes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Failure {
    /// Worth another try: no answer, a cut transfer, a server error, too many requests.
    Transient(String),
    /// Not worth one: a 404, a refused address, a disk that would not take the bytes.
    Permanent(String),
}

impl Failure {
    pub fn said(&self) -> &str {
        match self {
            Failure::Transient(it) | Failure::Permanent(it) => it,
        }
    }
}

/// What the driver does next.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Decision {
    /// Ask now; `attempt` from 1.
    Ask { attempt: u32 },
    /// Nothing until the clock reads `until`.
    Wait { until: Duration },
    /// Stop: `tries` were made, the last failing as `last` (permanently, or the last allowed).
    GiveUp { tries: u32, last: Failure },
}

/// The tries of one download.
#[derive(Debug)]
pub struct Attempts {
    policy: Policy,
    now: Duration,
    tried: u32,
    /// When the next try may start, after a transient failure.
    wait_until: Option<Duration>,
    last: Option<Failure>,
    asked: bool,
}

impl Attempts {
    pub fn new(policy: Policy) -> Attempts {
        Attempts {
            policy,
            now: Duration::ZERO,
            tried: 0,
            wait_until: None,
            last: None,
            asked: false,
        }
    }

    pub fn advance_clock(&mut self, now: Duration) {
        self.now = self.now.max(now);
    }

    /// What to do at the clock's time.
    pub fn next_decision(&mut self) -> Decision {
        if let Some(last) = &self.last {
            let out_of_tries = self.tried >= self.policy.tries.max(1);
            if matches!(last, Failure::Permanent(_)) || out_of_tries {
                return Decision::GiveUp { tries: self.tried, last: last.clone() };
            }
        }
        if self.asked {
            panic!("blaze-fetch's driver asked for a decision while a try is under way");
        }
        match self.wait_until {
            Some(until) if self.now < until => Decision::Wait { until },
            _ => {
                self.wait_until = None;
                self.asked = true;
                self.tried += 1;
                Decision::Ask { attempt: self.tried }
            }
        }
    }

    /// The try under way failed as `failure`.
    pub fn failed(&mut self, failure: Failure) {
        self.asked = false;
        if matches!(failure, Failure::Transient(_)) {
            self.wait_until = Some(self.now.saturating_add(self.policy.wait(self.tried)));
        }
        self.last = Some(failure);
    }

    /// When the driver must wake to ask again, if it is waiting.
    pub fn next_deadline(&self) -> Option<Duration> {
        self.wait_until
    }
}

/// Whether what came is what was pinned.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    Matches,
    /// `actual` is in the pin's algorithm, to be printed so it can be pasted, as Nix prints
    /// `got:` under `specified:`.
    Mismatch {
        expected: Digest,
        actual: Digest,
    },
}

pub fn verdict(expected: &Digest, got: &Hashed) -> Verdict {
    match got.digest == *expected {
        true => Verdict::Matches,
        false => Verdict::Mismatch { expected: *expected, actual: got.digest },
    }
}
