//! The daemon's side of what is built ahead (`serve::ahead`, the core; docs/v2/plan/design/
//! on-save.md): the loop hands the core what it reads and hears, and carries out what the
//! core decides. No decision is made here.

use super::*;
use crate::serve::ahead as decide;

/// A build ahead under way: what the daemon's log says of it as it ends.
pub(super) struct Run {
    checkout: String,
    request: u64,
    /// When it was sent, on the driver's clock.
    began: Duration,
    /// What its `Work` said, when it said one: the units it made again and its misses.
    made: Option<(u64, u64)>,
    cancelled: bool,
    /// From its request sent to its `Done`.
    _span: tracing::Span,
}

/// The daemon as the core of [`decide`] asks of it.
struct AheadWorld<'a, S: Steps> {
    jobs: &'a Jobs<S>,
    heads: &'a Heads,
    vfs: &'a Vfs,
    stopping: bool,
}

impl<S: Steps> decide::World for AheadWorld<'_, S> {
    fn idle(&self) -> bool {
        !self.stopping
            && self.jobs.idle()
            && self.jobs.placing_after() == 0
            && self.heads.waiting() == 0
    }

    fn head(&self, head: &str) -> Option<ContentId> {
        self.vfs.head(head)
    }

    fn known(&self, head: &str, root: ContentId) -> bool {
        self.jobs.answer_known(head, &root)
    }
}

impl<D: Driver, S: Steps> Daemon<D, S> {
    /// Whether a save is built before it is asked for ([`decide`]): set once, as the
    /// daemon starts, before any request.
    pub fn build_ahead(&mut self, on: bool) {
        self.ahead = decide::Ahead::new(on);
    }

    /// What the core of [`decide`] counted.
    #[cfg(test)]
    pub(in crate::serve) fn built_ahead(&self) -> decide::Counts {
        self.ahead.counts
    }

    /// Whether the core holds `arrived`, a client's request its watchers let through: one
    /// for what is being built ahead, which waits for that build, its own, and is handed on
    /// as it ends ([`Daemon::ahead_do`]). A build or a check of a checkout is noted there as
    /// what a save of the checkout builds ahead from now on.
    pub(super) fn ahead_holds(&mut self, arrived: &Arrived) -> bool {
        let Arrived::Request { from, request } = arrived else { return false };
        if !self.ahead.on() {
            return false;
        }
        let root = decide::built_ahead(request).and_then(|head| self.vfs.head(head));
        let held = self.ahead.released(self.driver.now(), *from, request, root);
        if held {
            crate::serve::said::logged!(
                INFO,
                "blaze serve: request {} waits for the build ahead under way, which is its own",
                from.id
            );
        }
        held
    }

    /// The turn's look at what is built ahead ([`decide::Ahead::tick`]): a build ahead
    /// owed is sent once nothing else is under way, and what the core decided is carried out.
    pub(super) fn ahead_turn(&mut self) {
        if !self.ahead.on() && !self.ahead.pending() {
            return;
        }
        let now = self.driver.now();
        let world = AheadWorld {
            jobs: &self.jobs,
            heads: &self.heads,
            vfs: &self.vfs,
            stopping: self.stopping.is_some() || self.stop_asked,
        };
        self.ahead.tick(now, &world);
        self.ahead_do();
    }

    /// Carries out what the core of [`decide`] decided, in order: its own request
    /// handed to the session as a client's is (with no watcher asked: what a build ahead
    /// makes is keyed by what it read, and the request after it waits for its own flush), a
    /// request it held handed on, its answer sent.
    pub(super) fn ahead_do(&mut self) {
        while let Some(it) = self.ahead.take() {
            match it {
                decide::Do::Send { id, request } => {
                    let from = RequestRef { conn: decide::CONN, id };
                    match decide::built_ahead(&request) {
                        Some(checkout) => {
                            let span = tracing::info_span!(
                                target: "blaze",
                                "build ahead: a save built before it is asked for",
                                checkout,
                                request = id,
                            );
                            self.ahead_run = Some(Run {
                                checkout: checkout.to_owned(), // alloc: once a build ahead, for its line
                                request: id,
                                began: self.driver.now(),
                                made: None,
                                cancelled: false,
                                _span: span,
                            });
                            self.changed_since_settle = true;
                        }
                        // Its own cancel of the build ahead under way.
                        None => {
                            if let Some(run) = &mut self.ahead_run {
                                run.cancelled = true;
                            }
                        }
                    }
                    self.to_session(Arrived::Request { from, request });
                }
                decide::Do::Pass { from, request } => {
                    self.to_session(Arrived::Request { from, request });
                }
                decide::Do::Answer(reply) => self.reply(&reply),
            }
        }
    }

    /// `arrived` handed to the session, and what the session asked for started.
    fn to_session(&mut self, arrived: Arrived) {
        let mut replies = std::mem::take(&mut self.replies);
        self.session.arrive(&mut cx!(self), arrived, &mut replies);
        self.replies = replies;
        self.after_session();
    }

    /// A reply to a request of the daemon's own ([`decide::CONN`]): told to the core, and
    /// at the end of a build ahead one line of the daemon's log says what it was.
    pub(super) fn ahead_heard(&mut self, reply: &Reply) {
        let of_run = self.ahead_run.as_ref().is_some_and(|run| run.request == reply.to.id);
        self.ahead.replied(reply.to.id, &reply.response);
        match &reply.response {
            blaze_api::Response::Work(worked) if of_run => {
                if let Some(run) = &mut self.ahead_run {
                    run.made = Some((worked.compiled, worked.misses));
                }
            }
            blaze_api::Response::Done { ok } if of_run => {
                if let Some(run) = self.ahead_run.take() {
                    let ms = self.driver.now().saturating_sub(run.began).as_secs_f64() * 1e3;
                    let how = match (run.cancelled, *ok) {
                        (true, _) => "cancelled",
                        (false, true) => "ok",
                        (false, false) => "failed",
                    };
                    let made = match run.made {
                        Some((compiled, misses)) => format!("{compiled} units made, {misses} misses"), // alloc: the line
                        None => "no work said".to_owned(), // alloc: the line
                    };
                    let counts = self.ahead.counts;
                    crate::serve::said::logged!(
                        INFO,
                        "blaze serve: build ahead of {} {how} in {ms:.1} ms: {made} (sent {}, ended {}, cancelled {}, requests held {})",
                        run.checkout,
                        counts.started,
                        counts.ended,
                        counts.cancelled,
                        counts.held,
                    );
                }
            }
            _ => {}
        }
    }

    /// A checkout's head moved to `root`: told to the core with how many files the move
    /// changed, counted from the root it had (which the tree holds until its next
    /// collection; one it no longer holds counts as more than any bound).
    pub(super) fn ahead_moved(&mut self, head: &str, root: ContentId) {
        if !self.ahead.on() {
            return;
        }
        let changed = match self.ahead.root_of(head) {
            Some(was) if was == root => 0,
            Some(was) if self.vfs.has(was) && self.vfs.has(root) => {
                let _span = tracing::info_span!(target: "blaze", "build ahead: count the files a save changed").entered();
                self.vfs.diff(was, root).len()
            }
            Some(_) => usize::MAX,
            // No client has built this checkout: nothing of it is built ahead.
            None => return,
        };
        let was = self.ahead.counts.mass;
        self.ahead.moved(self.driver.now(), head, root, changed);
        if self.ahead.counts.mass != was {
            crate::serve::said::logged!(
                INFO,
                "blaze serve: {head} moved by more than {} files: nothing of it is built ahead until it is built",
                decide::MASS
            );
        }
    }
}
