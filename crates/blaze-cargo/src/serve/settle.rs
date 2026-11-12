//! When the daemon's store ends a period of reading and moves what went unread cold
//! (`blaze_cas::Store::end_period`, `settle_take`, `settle_put`, `settle_step`): the rule
//! ([`Policy`], a core on the daemon's clock) and the batches out being compressed
//! ([`Settles`]). The daemon's loop asks between requests ([`super::daemon::Daemon`],
//! `settle_turn`); it never compresses: a batch goes to a worker and comes back over a channel.
//!
//! The rule. A period ends between requests once a build has ended since the last period's
//! end: what every build reads is then read in every period, and never goes cold. With no
//! build, a period ends [`PERIOD_IDLE`] after the last, at most [`IDLE_PERIODS`] times running:
//! as many as it takes for what the last build put to be old enough to move (a record stays
//! hot for the two periods after it was put), and no more, so a daemon left idle for a day has
//! ended three periods and not ten thousand. What a build after that reads cold it decompresses
//! once; read in two periods running, the value moves back hot.
//!
//! What it costs the loop: a period's end walks the store's rows once (a span,
//! `store period ends`), a take copies at most [`TAKE_MOST`] bytes out of the pool
//! (`store cold take`), a put copies the compressed bytes into the store's segment
//! (`store cold put`). zstd runs on a worker (`store cold compress`).

use std::time::Duration;

use blaze_cas::ColdBatch;
use blaze_graph::pool::Class;
use crossbeam_channel::{Receiver, Sender};

/// With no build since, a period ends this long after the last.
pub(super) const PERIOD_IDLE: Duration = Duration::from_secs(2);
/// How many periods end running with no build between them: a record put by the last build is
/// hot for the two periods after it, and the third's end moves it.
pub(super) const IDLE_PERIODS: u32 = 3;
/// The bytes of values one batch takes out of the store: what the loop copies in one turn
/// (and checks against their hashes, the first time each is read), and one worker compresses.
pub(super) const TAKE_MOST: u64 = 2 << 20;
/// The bytes of values moved after which the store is flushed: each flush ends its packs on a
/// block and syncs them, so not after every batch.
pub(super) const FLUSH_EVERY: u64 = 32 << 20;
/// The bytes one step of what follows the cold moves (moves back hot, half-dead packs
/// rewritten) may copy on the loop.
pub(super) const STEP_MOST: u64 = 8 << 20;

/// When a period of reading ends: the rule above, on the daemon's clock.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Policy {
    /// A build ended since the last period's end.
    build_ended: bool,
    /// Periods ended since the last build's.
    idle_periods: u32,
    /// When the last period ended.
    last: Option<Duration>,
}

impl Policy {
    /// A build ended (its job is done and answered).
    pub(super) fn build_ended(&mut self) {
        self.build_ended = true;
    }

    /// Whether a period ends at `now`, asked between requests.
    pub(super) fn due(&self, now: Duration) -> bool {
        if self.build_ended {
            return true;
        }
        match self.last {
            // Nothing was built since the daemon started: nothing was put, nothing to age.
            None => false,
            Some(last) => {
                self.idle_periods < IDLE_PERIODS && now.saturating_sub(last) >= PERIOD_IDLE
            }
        }
    }

    /// A period ended at `now`.
    pub(super) fn ended(&mut self, now: Duration) {
        self.idle_periods = match std::mem::take(&mut self.build_ended) {
            true => 0,
            false => self.idle_periods + 1,
        };
        self.last = Some(now);
    }

    /// When the next period ends if nothing is built, for the loop's wait.
    pub(super) fn next(&self) -> Option<Duration> {
        match self.last {
            Some(last) if !self.build_ended && self.idle_periods < IDLE_PERIODS => {
                Some(last + PERIOD_IDLE)
            }
            _ => None,
        }
    }
}

/// Where a batch is compressed.
pub enum Compress {
    /// On the worker pool, behind every build's work.
    Pool,
    /// Where it is asked: the simulation's daemons, which run on one thread.
    Inline,
}

/// The store's cold moves under way: the rule, and the batch out being compressed.
pub struct Settles {
    pub(super) policy: Policy,
    compress: Compress,
    /// A batch is with a worker.
    out: bool,
    /// Bytes of values moved since the store was last flushed for them.
    pub(super) unflushed: u64,
    /// A batch compressed, or `None` for one a panic on its worker lost.
    send: Sender<Option<ColdBatch>>,
    recv: Receiver<Option<ColdBatch>>,
}

impl Settles {
    pub fn new(compress: Compress) -> Settles {
        let (send, recv) = crossbeam_channel::unbounded();
        Settles { policy: Policy::default(), compress, out: false, unflushed: 0, send, recv }
    }

    /// Whether a batch is out being compressed.
    pub(super) fn out(&self) -> bool {
        self.out
    }

    /// `batch` handed to be compressed; it comes back through [`Settles::back`].
    pub(super) fn compress(&mut self, mut batch: ColdBatch) {
        self.out = true;
        // clone: a channel's sender, once a batch
        let send = self.send.clone();
        let work = move || {
            let _span = tracing::info_span!(
                target: "blaze",
                "store cold compress",
                kind = batch.kind(),
                values = batch.len(),
                bytes = batch.bytes(),
            )
            .entered();
            batch.compress();
            batch
        };
        match self.compress {
            Compress::Inline => _ = send.send(Some(work())),
            Compress::Pool => blaze_graph::exec::shared().submit_as(
                None,
                Class::Background,
                work,
                // A panic in zstd's binding fails this one job (panics-fail-one-job): the
                // batch is lost, its values stay as they are, and the next period's end plans
                // them again.
                move |done| _ = send.send(done.ok()),
            ),
        }
    }

    /// The batch that came back compressed, if one did. A batch a panic lost is said in the
    /// log and gives none; either way no batch is out after.
    pub(super) fn back(&mut self) -> Option<ColdBatch> {
        let back = self.recv.try_recv().ok()?;
        self.out = false;
        if back.is_none() {
            tracing::error!(
                target: "blaze",
                "a batch of the store's cold values was lost to a panic while it was compressed: its values stay as they are until the next period's end"
            );
        }
        back
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn at(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    /// The rule of the module's head: a period for each build, then three more two seconds
    /// apart, then none until the next build.
    #[test]
    fn a_period_ends_for_each_build_and_three_times_more_while_idle() {
        let mut policy = Policy::default();
        assert!(!policy.due(at(0)) && !policy.due(at(60_000)), "nothing built: no period ends");
        assert_eq!(policy.next(), None);
        policy.build_ended();
        assert!(policy.due(at(100)));
        policy.ended(at(100));
        assert!(!policy.due(at(2099)), "an idle period waits for its two seconds");
        assert_eq!(policy.next(), Some(at(2100)));
        let mut ends = Vec::new();
        for ms in (100..60_000).step_by(50) {
            if policy.due(at(ms)) {
                policy.ended(at(ms));
                ends.push(ms);
            }
        }
        assert_eq!(ends, [2100, 4100, 6100], "three idle periods, then none");
        assert_eq!(policy.next(), None, "the loop waits for no period");
        // Builds one after another: a period each, however close, and the idle count starts
        // over.
        for ms in [70_000, 70_010, 70_020] {
            policy.build_ended();
            assert!(policy.due(at(ms)));
            policy.ended(at(ms));
        }
        assert!(!policy.due(at(71_000)) && policy.due(at(72_020)));
    }
}
