//! Declared intent ([`crate::Request::WillTest`], [`crate::Request::WillSave`]): the jobs the
//! session started through [`Builder::speculate`], each on a view it holds until the job
//! finishes.
//!
//! An intent is taken over (promoted, once) by a request that builds its root or a commit to
//! its head that makes its root. One not taken over is cancelled by a newer intent of its
//! connection, by a commit to its head that makes another root, by its connection's end, or
//! when its deadline passes. A taken-over intent runs to its end.

use blaze_core::Conn;
use blaze_ids::ContentId;

use crate::build::{Builder, JobId, Opened};

struct Intent {
    conn: Conn,
    opened: Opened,
    /// The head a [`crate::Request::WillSave`] will move.
    head: Option<String>,
    /// Nanoseconds on the daemon's clock; `None` once taken over.
    deadline_ns: Option<u64>,
    cancelled: bool,
}

impl Intent {
    /// Neither taken over nor cancelled.
    fn open(&self) -> bool {
        self.deadline_ns.is_some() && !self.cancelled
    }
}

#[derive(Default)]
pub(super) struct Intents {
    /// Every intent whose job has not finished, in the order their jobs started, which is the
    /// order they are taken over and cancelled in ([`JobId`]s don't follow it). A list, scanned:
    /// one intent per waiting client, a few at once.
    started: Vec<(JobId, Intent)>,
    /// The daemon's clock as [`super::Session::tick`] last gave it.
    now_ns: u64,
}

impl Intents {
    /// `job`, started on `opened`, after the connection's intents not taken over are cancelled.
    pub(super) fn add(
        &mut self,
        // dyn: the daemon's one builder, lent by the session as `dyn`; one call per job.
        builder: &mut dyn Builder,
        conn: Conn,
        job: JobId,
        opened: Opened,
        head: Option<String>,
        wait_ms: u64,
    ) {
        self.cancel_where(builder, |intent| intent.conn == conn);
        let deadline_ns = self.now_ns.saturating_add(wait_ms.saturating_mul(1_000_000));
        let intent = Intent { conn, opened, head, deadline_ns: Some(deadline_ns), cancelled: false };
        self.started.push((job, intent));
    }

    /// A request builds `root`: every open intent on it is taken over.
    // dyn: as `add`'s.
    pub(super) fn take_over(&mut self, builder: &mut dyn Builder, root: ContentId) {
        self.promote_where(builder, |intent| intent.opened.view.root == root);
    }

    /// `head` moved to `root`: its open intents on `root` are taken over, the rest cancelled.
    // dyn: as `add`'s.
    pub(super) fn moved(&mut self, builder: &mut dyn Builder, head: &str, root: ContentId) {
        self.cancel_where(builder, |intent| {
            intent.head.as_deref() == Some(head) && intent.opened.view.root != root
        });
        self.promote_where(builder, |intent| {
            intent.head.as_deref() == Some(head) && intent.opened.view.root == root
        });
    }

    // dyn: as `add`'s.
    pub(super) fn gone(&mut self, builder: &mut dyn Builder, conn: Conn) {
        self.cancel_where(builder, |intent| intent.conn == conn);
    }

    /// The clock is `now_ns`: every open intent whose deadline it reached is cancelled.
    // dyn: as `add`'s.
    pub(super) fn tick(&mut self, builder: &mut dyn Builder, now_ns: u64) {
        self.now_ns = self.now_ns.max(now_ns);
        let now_ns = self.now_ns;
        self.cancel_where(builder, |intent| intent.deadline_ns.is_some_and(|it| it <= now_ns));
    }

    /// The earliest deadline of an open intent.
    /// A scan, not a timing wheel: one intent per waiting client, a few at once.
    pub(super) fn deadline(&self) -> Option<u64> {
        self.started.iter().filter(|(_, it)| it.open()).filter_map(|(_, it)| it.deadline_ns).min()
    }

    /// The view of `job` when it is an intent's, the intent removed; the rest keep their order.
    pub(super) fn finish(&mut self, job: JobId) -> Option<Opened> {
        let at = self.started.iter().position(|(it, _)| *it == job)?;
        Some(self.started.remove(at).1.opened)
    }

    pub(super) fn views(&self) -> usize {
        self.started.len()
    }

    /// Takes over the open intents `keep` picks, in start order.
    // dyn: as `add`'s.
    fn promote_where(&mut self, builder: &mut dyn Builder, mut keep: impl FnMut(&Intent) -> bool) {
        for (job, intent) in &mut self.started {
            if intent.open() && keep(&*intent) {
                intent.deadline_ns = None;
                builder.promote(*job);
            }
        }
    }

    /// Cancels the open intents `keep` picks, in start order.
    // dyn: as `add`'s.
    fn cancel_where(&mut self, builder: &mut dyn Builder, mut keep: impl FnMut(&Intent) -> bool) {
        for (job, intent) in &mut self.started {
            if intent.open() && keep(&*intent) {
                intent.cancelled = true;
                builder.cancel(*job);
            }
        }
    }
}

#[cfg(test)]
mod tests;
