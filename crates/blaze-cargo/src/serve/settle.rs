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
//! What it costs the loop: a period's end hands a scan out (a span, `store period ends`) and
//! takes its plan back (`store period plan`), neither walking a row: the walk of the store's
//! rows is the scan's, on a worker (`store period scan`), so the loop's cost does not grow
//! with the store. A take names at most [`TAKE_MOST`] bytes of values (`store cold take`), a
//! put copies the compressed bytes into the store's segment (`store cold put`). zstd runs on
//! a worker (`store cold compress`). Packs that are half dead bytes are rewritten by the
//! store's collector ([`super::collect`]), a bounded copy a turn, not here.

use std::time::Duration;

use blaze_cas::{
    ColdBatch,
    collect::{FlagCarry, PeriodScan},
};
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
/// The bytes one step of what follows the cold moves (moves back hot) may copy on the loop:
/// with the collector's copies, the loop's bound of a millisecond a step (docs/gc.md).
pub(super) const STEP_MOST: u64 = 256 << 10;

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
    /// A period's scan is with a worker.
    scanning: bool,
    /// A scan that ran, or `None` for one a panic on its worker lost.
    scan_send: Sender<Option<PeriodScan>>,
    scan_recv: Receiver<Option<PeriodScan>>,
    /// The flags of use of an index swap are with a worker, being carried to the new table.
    carrying: bool,
    carry_send: Sender<Option<FlagCarry>>,
    carry_recv: Receiver<Option<FlagCarry>>,
}

impl Settles {
    pub fn new(compress: Compress) -> Settles {
        let (send, recv) = crossbeam_channel::unbounded();
        let (scan_send, scan_recv) = crossbeam_channel::unbounded();
        let (carry_send, carry_recv) = crossbeam_channel::unbounded();
        Settles {
            policy: Policy::default(),
            compress,
            out: false,
            unflushed: 0,
            send,
            recv,
            scanning: false,
            scan_send,
            scan_recv,
            carrying: false,
            carry_send,
            carry_recv,
        }
    }

    /// Whether an index swap's flags are out being carried over.
    pub(super) fn carrying(&self) -> bool {
        self.carrying
    }

    /// `carry` handed to be run (`store flags carry`: a lookup a record of the new index
    /// table, so it is a worker's); it comes back through [`Settles::carried`].
    pub(super) fn carry(&mut self, mut carry: FlagCarry) {
        self.carrying = true;
        // clone: a channel's sender, once an index swap
        let send = self.carry_send.clone();
        let work = move || {
            let span = tracing::info_span!(
                target: "blaze",
                "store flags carry",
                carried = tracing::field::Empty,
            )
            .entered();
            carry.run();
            span.record("carried", carry.carried());
            carry
        };
        match self.compress {
            Compress::Inline => _ = send.send(Some(work())),
            Compress::Pool => blaze_graph::exec::shared().submit_as(
                None,
                Class::Background,
                work,
                // A panic fails this one job: the store counts its periods from 0 again,
                // as after an open.
                move |done| _ = send.send(done.ok()),
            ),
        }
    }

    /// The carry that came back, if one did (`Some(None)`: lost to a panic).
    pub(super) fn carried(&mut self) -> Option<Option<FlagCarry>> {
        let back = self.carry_recv.try_recv().ok()?;
        self.carrying = false;
        Some(back)
    }

    /// Whether a period's scan is out being run.
    pub(super) fn scanning(&self) -> bool {
        self.scanning
    }

    /// `scan` handed to be run; it comes back through [`Settles::scanned`].
    pub(super) fn scan(&mut self, mut scan: PeriodScan) {
        self.scanning = true;
        // clone: a channel's sender, once a scan
        let send = self.scan_send.clone();
        let work = move || {
            let span = tracing::info_span!(
                target: "blaze",
                "store period scan",
                records = tracing::field::Empty,
            )
            .entered();
            scan.run();
            span.record("records", scan.records());
            scan
        };
        match self.compress {
            Compress::Inline => _ = send.send(Some(work())),
            Compress::Pool => blaze_graph::exec::shared().submit_as(
                None,
                Class::Background,
                work,
                // A panic fails this one job (panics-fail-one-job): the scan is lost, the
                // store keeps the plan and the mark it had, and the next period's end scans
                // again.
                move |done| _ = send.send(done.ok()),
            ),
        }
    }

    /// The scan that came back, if one did. One a panic lost is said in the log and gives
    /// none; either way no scan is out after.
    pub(super) fn scanned(&mut self) -> Option<Option<PeriodScan>> {
        let back = self.scan_recv.try_recv().ok()?;
        self.scanning = false;
        if back.is_none() {
            tracing::error!(
                target: "blaze",
                "the scan of a period's end of the store was lost to a panic: the store keeps the plan it had until the next period's end"
            );
        }
        Some(back)
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
