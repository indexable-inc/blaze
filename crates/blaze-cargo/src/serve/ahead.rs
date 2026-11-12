//! Built at the save (docs/v2/plan/design/on-save.md): what the daemon builds before it is
//! asked, decided here and carried out by the daemon's loop (`Daemon::ahead_turn`).
//!
//! For each checkout the core keeps the last build or check a client sent of it, whole. When
//! the checkout's head moves (the watcher committed a save) and no answer of that request is
//! kept for the new root, the core has the daemon send itself that request again, unchanged,
//! on a connection of the daemon's own ([`CONN`]): a *build ahead*. It is the client's
//! request and goes where a client's goes (the session, the no-op lane, a job), so nothing it
//! does is a state two commands typed in a row do not reach. Its replies come back here
//! ([`Ahead::replied`]) and go nowhere else.
//!
//! The rules, each held by the schedule test beside this file:
//!
//! - **One.** At most one build ahead is under way in the daemon, and one starts only while
//!   nothing else is: no job, no request held for a watcher, no output being placed
//!   ([`World::idle`]).
//! - **It yields.** A request that reads a checkout or builds, read while a build ahead is
//!   under way, cancels it as it is read ([`Ahead::read`]) unless it is the very request the
//!   build ahead is. A request is never held behind a build ahead of anything else, and
//!   never waits for a cancelled one to end.
//! - **The same request waits for its own build.** A request equal to the build ahead's, let
//!   through by its watcher while the head is at the root the build ahead is of, is held
//!   until the build ahead ends and then handed to the session ([`Do::Pass`]), where it walks
//!   and finds the units made. Its answer is its own job's, never the build ahead's.
//! - **A newer save cancels an older build ahead** ([`Ahead::moved`]).
//! - **Bounded.** A root is built ahead at most [`STARTS`] times, of which at most [`ENDS`]
//!   run to their end (one that makes, one that walks and whose answer the no-op lane keeps);
//!   one that fails is not started again: its diagnostics are in its units' records. A head
//!   that moved by more than [`MASS`] files (a branch switch) is not built ahead until a
//!   client has built it.
//! - **Only a client's build or check.** What is sent is a [`Request::Build`] or a
//!   [`Request::Check`] a client sent of the checkout, equal to it: it runs the build scripts
//!   and proc macros that request ran, sandboxed as they were, and no program and no test.
//!
//! A sans-IO core (rule 14, marked `#[blaze::sans_io]` where `serve` declares it): it reads
//! no file, no clock and no environment; the daemon hands it the clock with each call, and
//! what it cannot know (whether the daemon is idle, where a head is, whether an answer is
//! kept) through [`World`].

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_api::{Arrived, Reply, Request, RequestRef, Response, ViewRef};
use blaze_core::Conn;
use blaze_ids::ContentId;

/// The connection the daemon's own requests are of: no driver names a connection so, and
/// every reply to it is the core's ([`Ahead::replied`]), never the server's.
pub(super) const CONN: Conn = Conn(u64::MAX);

/// How long a checkout's head has stood still before it is built ahead: a save by
/// write-then-rename, or of two files at once, is commits a millisecond apart, and each
/// would start and cancel a build.
pub(super) const QUIET: Duration = Duration::from_millis(5);

/// The most files a move of a head may change for it to be built ahead: past it the move is
/// a branch switch or a rebase, whose build is seconds of every core that nobody asked for.
pub(super) const MASS: usize = 64;

/// The most builds ahead started of one root of one checkout (rule 18): a request of another
/// checkout may cancel one, and it is started again, this often.
pub(super) const STARTS: u8 = 4;

/// The most builds ahead of one root that run to their end: one that makes what the save
/// changed, and one that walks, makes nothing, and leaves the answer the no-op lane keeps.
pub(super) const ENDS: u8 = 2;

/// What the core cannot know of the daemon, asked as it decides ([`Ahead::tick`]).
pub(super) trait World {
    /// Whether nothing else is under way: no job, no request held for a watcher, no output
    /// of an answered build being placed, and the daemon not stopping.
    fn idle(&self) -> bool;
    /// The root `head` is at now.
    fn head(&self, head: &str) -> Option<ContentId>;
    /// Whether the checkout's kept answer is known to be of `root`
    /// (`Jobs::answer_known`): the next build or check of it is answered by the no-op lane.
    fn known(&self, head: &str, root: ContentId) -> bool;
}

/// What the daemon does for the core, in order ([`Ahead::take`]).
#[derive(Debug, PartialEq)]
pub(super) enum Do {
    /// `request` arrives at the session as request `id` of [`CONN`].
    Send { id: u64, request: Request },
    /// A client's request the core held goes to the session now.
    Pass { from: RequestRef, request: Request },
    /// The core's own answer to a client: a held request cancelled.
    Answer(Reply),
}

/// Why a checkout's root is not built ahead.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Off {
    /// A build ahead of it failed: its diagnostics wait in its units' records.
    Failed,
    /// The head moved by more than [`MASS`] files since a client built it.
    Mass,
    /// [`STARTS`] began, or [`ENDS`] ended, and no answer of it is kept: something the
    /// no-op lane does not cover is read by every build of it.
    Spent,
}

/// A bug a test plants in the core, which its oracle has to catch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Plant {
    /// Starts a build ahead whatever else is under way, another build ahead too.
    TwoJobs,
    /// Holds every build or check of a checkout behind its build ahead, whatever the request
    /// and the root, and cancels none.
    Queues,
    /// Answers a held request with how the build ahead ended, of the root it was of, and
    /// holds a request behind a build ahead of an older root.
    OldAnswer,
    /// Builds ahead something other than what the client sent: a check for its build.
    OtherShape,
}

#[cfg(test)]
thread_local! {
    /// The bug the test on this thread planted.
    pub(super) static PLANTED: std::cell::Cell<Option<Plant>> = const { std::cell::Cell::new(None) };
}

/// Whether the test on this thread planted `plant`; never outside a test.
fn planted(plant: Plant) -> bool {
    #[cfg(test)]
    let planted = PLANTED.get() == Some(plant);
    #[cfg(not(test))]
    let planted = {
        _ = plant;
        false
    };
    planted
}

struct Checkout {
    head: String,
    /// The last build or check a client sent of it.
    last: Request,
    /// The root its head was at when the core last heard: a move, or a request let through.
    root: Option<ContentId>,
    /// When its head last moved, on the daemon's clock.
    moved: Duration,
    /// Its root may have no kept answer: a build ahead is owed.
    owed: bool,
    off: Option<Off>,
    /// The builds ahead of `root` begun, and those that ran to their end.
    starts: u8,
    ends: u8,
    /// The files its head's moves changed since a build of it last ended well or a client
    /// last built it: a branch switch heard as many small commits is still one of [`MASS`].
    changed: usize,
}

/// The build ahead under way.
struct Running {
    /// Its request's id on [`CONN`].
    id: u64,
    /// Its checkout, by place in [`Ahead::checkouts`].
    checkout: usize,
    /// The root its head was at as it was sent.
    root: ContentId,
    cancelled: bool,
}

/// What the core counted, for the daemon's log and the tests.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(super) struct Counts {
    /// Builds ahead sent.
    pub(super) started: u64,
    /// Those that ran to their end, ok or not.
    pub(super) ended: u64,
    /// Those cancelled: for a request, for a newer save, for a stop.
    pub(super) cancelled: u64,
    /// Requests held behind the build ahead that was their own.
    pub(super) held: u64,
    /// Roots found with their answer kept as the core looked: nothing to build.
    pub(super) known: u64,
    /// Moves of a head past [`MASS`].
    pub(super) mass: u64,
}

pub(super) struct Ahead {
    on: bool,
    stopped: bool,
    /// A list, scanned: a checkout a project someone works in, a few a daemon.
    checkouts: Vec<Checkout>,
    running: Option<Running>,
    /// The requests held behind `running`, in the order their watchers let them through.
    held: Vec<(RequestRef, Request)>,
    next: u64,
    out: Vec<Do>,
    pub(super) counts: Counts,
}

/// The checkout a build or a check names, when it is one of a checkout as saved: what is
/// built ahead. A what-if view, an agent's head, and every other request (a test or a run
/// inside the daemon, documentation, a lock) is not.
pub(super) fn built_ahead(request: &Request) -> Option<&str> {
    match request {
        Request::Build { view: ViewRef::Head(head), .. }
        | Request::Check { view: ViewRef::Head(head), .. }
            if super::heads::is_checkout(head) =>
        {
            Some(head)
        }
        _ => None,
    }
}

/// Whether a build ahead gives way to `request`: everything that reads a checkout, builds,
/// moves a head or stops the daemon. What only asks the daemon about itself does not.
fn yielded_to(request: &Request) -> bool {
    !super::heads::named(request).is_empty()
        || super::heads::runs_user_code(request)
        || matches!(request, Request::Commit { .. } | Request::Snapshot | Request::Stop)
}

impl Ahead {
    /// A core that builds ahead when `on`, and otherwise does nothing at all.
    pub(super) fn new(on: bool) -> Ahead {
        Ahead {
            on,
            stopped: false,
            checkouts: Vec::new(), // alloc: none, empty
            running: None,
            held: Vec::new(), // alloc: none, empty
            next: 1,
            out: Vec::new(), // alloc: none, empty
            counts: Counts::default(),
        }
    }

    pub(super) fn on(&self) -> bool {
        self.on && !self.stopped
    }

    /// Whether a build ahead is under way.
    pub(super) fn running(&self) -> bool {
        self.running.is_some()
    }

    /// The root the core last heard `head` at, for the daemon to count a move's files from.
    pub(super) fn root_of(&self, head: &str) -> Option<ContentId> {
        self.checkouts.iter().find(|it| it.head == head)?.root
    }

    /// Whether anything waits for the daemon to carry out.
    pub(super) fn pending(&self) -> bool {
        !self.out.is_empty()
    }

    /// The next thing for the daemon to do, in the order decided.
    pub(super) fn take(&mut self) -> Option<Do> {
        if self.out.is_empty() { None } else { Some(self.out.remove(0)) }
    }

    fn id(&mut self) -> u64 {
        let id = self.next;
        self.next += 1;
        id
    }

    /// The build ahead under way is cancelled, once: a cancel of its request is sent, and
    /// every request held behind it is handed on.
    fn cancel(&mut self) {
        let Some(running) = &mut self.running else { return };
        if running.cancelled {
            return;
        }
        running.cancelled = true;
        let target = running.id;
        self.counts.cancelled += 1;
        let id = self.id();
        self.out.push(Do::Send { id, request: Request::Cancel { id: target } });
        // What was held behind it goes on now, beside its end: no request waits for a
        // cancelled build ahead to be over.
        for (from, request) in self.held.drain(..) {
            self.out.push(Do::Pass { from, request });
        }
    }

    /// Whether `request` is the request of the build ahead under way, not cancelled.
    fn same(&self, request: &Request) -> bool {
        self.running.as_ref().is_some_and(|running| {
            !running.cancelled && self.checkouts[running.checkout].last == *request
        })
    }

    /// Something read off a connection, before its watchers are asked: `true` when the core
    /// took it (a cancel of a request it holds, which it answers), and the daemon hands it
    /// no further.
    ///
    /// A request a build ahead gives way to cancels it here, as it is read, so the cancel
    /// runs beside the request's wait for its watcher; the request itself goes on as ever.
    pub(super) fn read(&mut self, arrived: &Arrived) -> bool {
        if !self.on {
            return false;
        }
        match arrived {
            Arrived::Gone { conn } => {
                self.held.retain(|(from, _)| from.conn != *conn);
                false
            }
            Arrived::Request { from, request: Request::Cancel { id } } => {
                let target = RequestRef { conn: from.conn, id: *id };
                let Some(at) = self.held.iter().position(|(it, _)| *it == target) else {
                    return false;
                };
                self.held.remove(at);
                // As the watchers' hold answers one: the request not ok, its cancel ok.
                for (to, ok) in [(target, false), (*from, true)] {
                    self.out.push(Do::Answer(Reply { to, response: Response::Done { ok } }));
                }
                true
            }
            Arrived::Request { request, .. } => {
                if yielded_to(request) && !self.same(request) && !planted(Plant::Queues) {
                    self.cancel();
                }
                false
            }
        }
    }

    /// A client's request its watchers let through, the head it builds at `root` now:
    /// `true` when the core holds it (behind the build ahead that is its own), to hand it on
    /// later ([`Do::Pass`]); else the daemon hands it to the session now.
    ///
    /// A build or a check of a checkout is noted as the checkout's last: what a save of it
    /// builds ahead from now on.
    pub(super) fn released(
        &mut self,
        now: Duration,
        from: RequestRef,
        request: &Request,
        root: Option<ContentId>,
    ) -> bool {
        if !self.on() {
            return false;
        }
        let Some(head) = built_ahead(request) else {
            if yielded_to(request) && !planted(Plant::Queues) {
                self.cancel();
            }
            return false;
        };
        let at = match self.checkouts.iter().position(|it| it.head == head) {
            Some(at) => at,
            None => {
                // alloc: once a checkout a client builds, its name and its request kept
                self.checkouts.push(Checkout {
                    head: head.to_owned(),
                    last: request.clone(), // clone: as above
                    root,
                    moved: now,
                    owed: false,
                    off: None,
                    starts: 0,
                    ends: 0,
                    changed: 0,
                });
                self.checkouts.len() - 1
            }
        };
        let own = self.running.as_ref().is_some_and(|running| {
            let fits = running.checkout == at
                && self.checkouts[at].last == *request
                && (Some(running.root) == root || planted(Plant::OldAnswer));
            (!running.cancelled && fits)
                || (planted(Plant::Queues) && running.checkout == at)
        });
        let checkout = &mut self.checkouts[at];
        if checkout.last != *request {
            checkout.last = request.clone(); // clone: once a request of another shape
        }
        if checkout.root != root {
            (checkout.root, checkout.moved) = (root, now);
        }
        // The request's own build may make something, and then keeps no answer: one walk
        // is owed after it (the request's is the build that makes). A client built this
        // root, so it is built ahead again, a mass move or a failure before it
        // notwithstanding.
        (checkout.owed, checkout.off, checkout.starts, checkout.ends) = (true, None, 0, 1);
        checkout.changed = 0;
        if own {
            self.counts.held += 1;
            self.held.push((from, request.clone())); // clone: once a request held
            return true;
        }
        if !planted(Plant::Queues) {
            self.cancel();
        }
        false
    }

    /// A watcher committed a save: `head` is at `root`, `changed` files from the root the
    /// core last heard it at (`usize::MAX` when they could not be counted). The files of the
    /// moves since the checkout was last built are summed against [`MASS`].
    pub(super) fn moved(&mut self, now: Duration, head: &str, root: ContentId, changed: usize) {
        if !self.on() {
            return;
        }
        let Some(at) = self.checkouts.iter().position(|it| it.head == head) else { return };
        let checkout = &mut self.checkouts[at];
        if checkout.root == Some(root) {
            return;
        }
        (checkout.root, checkout.moved, checkout.owed) = (Some(root), now, true);
        (checkout.starts, checkout.ends) = (0, 0);
        checkout.changed = checkout.changed.saturating_add(changed);
        if checkout.changed > MASS {
            if checkout.off != Some(Off::Mass) {
                self.counts.mass += 1;
            }
            checkout.off = Some(Off::Mass);
        } else if checkout.off != Some(Off::Mass) {
            // A failure and a spent bound were of the root before.
            checkout.off = None;
        }
        let stale = self
            .running
            .as_ref()
            .is_some_and(|running| running.checkout == at && running.root != root);
        if stale && !planted(Plant::OldAnswer) {
            self.cancel();
        }
    }

    /// A reply to request `id` of [`CONN`]. The end of the build ahead (its `Done`) hands
    /// on every request held behind it.
    pub(super) fn replied(&mut self, id: u64, response: &Response) {
        let Response::Done { ok } = response else { return };
        let Some(running) = self.running.take_if(|running| running.id == id) else { return };
        let checkout = &mut self.checkouts[running.checkout];
        if !running.cancelled {
            self.counts.ended += 1;
            checkout.ends = checkout.ends.saturating_add(1);
            if *ok {
                checkout.changed = 0;
            } else if checkout.root == Some(running.root) {
                checkout.off = Some(Off::Failed);
            }
        }
        for (from, request) in self.held.drain(..) {
            if planted(Plant::OldAnswer) {
                // The planted bug: the build ahead's end is the held request's answer.
                self.out.push(Do::Answer(Reply { to: from, response: Response::Done { ok: *ok } }));
            } else {
                self.out.push(Do::Pass { from, request });
            }
        }
    }

    /// The daemon stops: nothing is built ahead from now on, and the one under way is
    /// cancelled; what was held behind it goes to the session, which answers a stopping
    /// daemon's requests.
    pub(super) fn stop(&mut self) {
        if !self.stopped {
            self.cancel();
            self.stopped = true;
        }
    }

    /// A look at `now`: a build ahead is sent when one is owed, its checkout's head has
    /// stood still for [`QUIET`], and nothing else is under way.
    pub(super) fn tick(&mut self, now: Duration, world: &impl World) {
        if !self.on() || !self.checkouts.iter().any(|it| it.owed) {
            return;
        }
        let free = (self.running.is_none() && self.held.is_empty() && world.idle())
            || planted(Plant::TwoJobs);
        if !free {
            return;
        }
        for at in 0..self.checkouts.len() {
            let checkout = &mut self.checkouts[at];
            if !checkout.owed || now.saturating_sub(checkout.moved) < QUIET {
                continue;
            }
            // The head as it is, whoever moved it: a commit of a client's moves it too.
            let Some(root) = world.head(&checkout.head) else {
                checkout.owed = false;
                continue;
            };
            if checkout.root != Some(root) {
                (checkout.root, checkout.starts, checkout.ends) = (Some(root), 0, 0);
                if checkout.off != Some(Off::Mass) {
                    checkout.off = None;
                }
            }
            if checkout.off.is_some() {
                checkout.owed = false;
                continue;
            }
            if world.known(&checkout.head, root) {
                checkout.owed = false;
                self.counts.known += 1;
                continue;
            }
            if checkout.starts >= STARTS || checkout.ends >= ENDS {
                (checkout.owed, checkout.off) = (false, Some(Off::Spent));
                continue;
            }
            checkout.starts += 1;
            let request = match &checkout.last {
                Request::Build { view, targets } if planted(Plant::OtherShape) => {
                    // clone: the planted bug's request
                    Request::Check { view: view.clone(), targets: targets.clone() }
                }
                last => last.clone(), // clone: the request sent, once a build ahead
            };
            let id = self.id();
            self.running = Some(Running { id, checkout: at, root, cancelled: false });
            self.counts.started += 1;
            self.out.push(Do::Send { id, request });
            return;
        }
    }

    /// When the daemon's loop next has to turn for the core with nothing else happening:
    /// now, while something decided waits to be carried out (a request held behind a build
    /// ahead that just ended is handed on in this turn, not at the loop's idle tick); else
    /// the end of the earliest [`QUIET`] of a checkout owed a build ahead. A build ahead owed
    /// past its quiet waits on the daemon being idle, which a turn of its loop finds, not
    /// the clock.
    pub(super) fn deadline(&self, now: Duration) -> Option<Duration> {
        if !self.out.is_empty() {
            return Some(now);
        }
        if !self.on() || self.running.is_some() {
            return None;
        }
        self.checkouts
            .iter()
            .filter(|it| it.owed)
            .map(|it| it.moved + QUIET)
            .filter(|at| *at > now)
            .min()
    }

    /// Why `head`'s root is not built ahead, if it is not.
    #[cfg(test)]
    pub(super) fn off(&self, head: &str) -> Option<Off> {
        self.checkouts.iter().find(|it| it.head == head)?.off
    }
}

#[cfg(test)]
mod tests;
