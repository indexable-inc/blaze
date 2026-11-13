//! What the loop knows of each checkout's kept front end besides the front end itself: what it
//! took to make from nothing, whether its workspace members' pages are written, and the idle
//! stretch's trim.
//!
//! Idleness (`serve::idle`'s rule, 2026-11-13). Nothing is let go for idleness but what costs
//! nothing to make again: once the daemon has had no job for [`idle::AFTER`], blaze-mem's warm
//! chunks and the flight reader's past names, once a stretch ([`Jobs::idle_turn`]). No front
//! end, value, kept walk or link image is let go for idleness: the daemon exits instead, once it
//! has been idle 64 times the dearest kept front end's last remake (30 s to 30 minutes,
//! [`Jobs::exit_wait`], `idle::exit_after`), through `Daemon::stop`, which writes each kept
//! front end's members first ([`Jobs::store_all_members`]).
//!
//! Deleted with the memory machinery (2026-11-13): the give-back ladder that let go of kept
//! front ends, their values, codegen's kept walks and the allocator's pages as the memory
//! gate's core filled, and its simulation. The kernel's limit is the ceiling.

use std::{
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

use blaze_core::lock::Lock;
use blaze_graph::Pool;
use blaze_protocol::events::NodeKind;

use super::{Does, Jobs, Stage, Step, Steps, World, key_of};
use crate::serve::{idle, plane::kinds, plane::ns};

/// Where the pages of a checkout's members are, of the front end it keeps now
/// ([`Jobs::store_members`]).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
enum Members {
    /// Not written by this front end: its build's end stored the sealed crates alone.
    #[default]
    Unwritten,
    /// Its second store is under way, the front end out with it.
    Writing,
    /// Written, the front end back; durable once nothing is left to settle under
    /// [`members_job`].
    Written,
}

/// What the store of checkout `root`'s members is named by among what is left to settle
/// (`Settling`): it is no job's, and no job has this slot, so nothing else is under it.
fn members_job(root: u32) -> blaze_api::build::JobId {
    blaze_api::build::JobId { slot: u32::MAX, generation: root }
}

/// What the loop knows of one checkout's kept front end.
#[derive(Clone, Copy, Default)]
struct Root {
    /// When its lowering from nothing began, until its front end is kept.
    began: Option<Duration>,
    /// What its front end took to make from nothing, lowering to kept, the last time; 0 until
    /// one was.
    cost_ns: u64,
    /// Whether `cost_ns` was measured: a daemon that keeps a front end never made from nothing
    /// in it waits the longest before it exits ([`idle::EXIT_MOST`], [`Jobs::exit_wait`]).
    measured: bool,
    /// Whether its kept front end's members are in the store ([`Jobs::store_members`]).
    members: Members,
}

/// The loop's record of the kept front ends, the daemon's one.
pub(super) struct Fronts {
    /// By root number.
    roots: Vec<Root>,
    /// Since when the daemon has had no job.
    idle_since: Option<Duration>,
    /// The caches that cost nothing to make again were trimmed in this idle stretch
    /// ([`trim_idle`]).
    trimmed: bool,
}

impl Fronts {
    pub(super) fn new() -> Fronts {
        Fronts {
            roots: Vec::new(), // alloc: one entry a checkout
            idle_since: None,
            trimmed: false,
        }
    }

    fn root(&mut self, root: u32) -> &mut Root {
        let at = root as usize;
        if self.roots.len() <= at {
            self.roots.resize(at + 1, Root::default()); // alloc: one entry a checkout
        }
        &mut self.roots[at]
    }

    /// A job of checkout `root` lowers at `now`, into its kept front end (`kept`) or from
    /// nothing: the time a front end made from nothing takes is what the daemon's exit waits on.
    pub(super) fn lowers(&mut self, root: u32, kept: bool, now: Duration) {
        let it = self.root(root);
        it.began = (!kept).then_some(now);
        // The build writes into the front end: its members' pages are of the next build's end.
        it.members = Members::Unwritten;
    }

    /// Checkout `root`'s front end is kept again at `now`: one made from nothing records what
    /// it took.
    pub(super) fn kept(&mut self, root: u32, now: Duration) {
        // The store of its members, back.
        if self.root(root).members == Members::Writing {
            self.root(root).members = Members::Written;
            return;
        }
        if let Some(began) = self.root(root).began.take() {
            let cost_ns = ns(now.saturating_sub(began));
            *self.root(root) = Root { began: None, cost_ns, measured: true, members: Members::Unwritten };
        }
    }
}

impl<S: Steps> Jobs<S> {
    /// The checkout numbered `root`, by its path.
    fn checkout_of(&self, root: u32) -> &str {
        self.roots.ids.iter().find(|&(_, &id)| id == root).map_or("no checkout", |(name, _)| &**name)
    }

    /// How long the daemon waits, idle and held by no one, before it exits (`serve::idle`'s
    /// rule 2, `idle::exit_due`): [`idle::EXIT_UNWORKED`] where it built nothing and took no
    /// save since its start (`worked`); else [`idle::exit_after`] of what the next edit would
    /// pay again without it, the dearest kept front end's last lowering from nothing
    /// ([`idle::EXIT_MOST`] for one never measured, [`idle::EXIT_LEAST`] with none kept).
    pub fn exit_wait(&self, worked: bool) -> Duration {
        if !worked {
            return idle::EXIT_UNWORKED;
        }
        let mut dearest = Duration::ZERO;
        for (at, front) in self.roots.fronts.iter().enumerate() {
            if front.is_none() {
                continue;
            }
            let remake = match self.fronts.roots.get(at) {
                Some(root) if root.measured => Duration::from_nanos(root.cost_ns),
                _ => return idle::EXIT_MOST,
            };
            dearest = dearest.max(remake);
        }
        idle::exit_after(dearest)
    }

    /// Once a loop turn: since when the daemon has had no job, and, once an idle stretch past
    /// [`idle::AFTER`], the caches that cost nothing to make again trimmed ([`trim_idle`]).
    pub(super) fn idle_turn(&mut self, w: &World<'_, S>) {
        let jobless = !self.building() && self.order.is_empty();
        match (jobless, self.fronts.idle_since) {
            (true, None) => self.fronts.idle_since = Some(w.now),
            (false, Some(_)) => (self.fronts.idle_since, self.fronts.trimmed) = (None, false),
            _ => {}
        }
        let trim = self.fronts.idle_since.is_some_and(|since| w.now.saturating_sub(since) >= idle::AFTER);
        if trim && !std::mem::replace(&mut self.fronts.trimmed, true) {
            trim_idle();
        }
    }

    /// Whether a build's units are under way: a job at its units' stage.
    fn building(&self) -> bool {
        self.order.iter().any(|&id| {
            self.jobs.get(key_of(id)).is_some_and(|job| matches!(job.stage, Stage::Units(_)))
        })
    }

    /// Writes the pages of checkout `root`'s members, and of the crates above them, from its
    /// kept front end: the front end's second store ([`Steps::store_kept`] on one its build's
    /// end stored), by a task of an idle plane request of its own, as a build's end posts its
    /// first ([`Jobs::store_kept`]). No job's: it settles as what an answered job left does
    /// (`Settling`, named by [`members_job`]), its puts made by `stored_kept`, the front end
    /// the checkout's again once they are, and what is left to settle gone once they are
    /// durable; a lowering of the checkout meanwhile waits for it as for any store. Whether a
    /// task was posted: none when the checkout keeps no front end now, or the plane refused
    /// it (said; the front end is dropped with the task, as a refused store's is).
    pub(super) fn store_members(&mut self, w: &mut World<'_, S>, root: u32) -> bool {
        // A store that broke makes nothing durable again (the daemon stops, failed naming it):
        // pages put now could only be lost, and the loss of what answered builds left is said
        // already. The front end stays the checkout's, and goes with the daemon.
        if w.store.broken().is_some() {
            return false;
        }
        let Some(front) = self.roots.fronts.get_mut(root as usize).and_then(Option::take) else {
            return false;
        };
        let checkout = self.checkout_of(root).to_owned(); // alloc: a store's name, once a store
        let request = w.plane.open(w.now, false, true);
        let name = super::named("storing the members of the kept front end of ", &checkout);
        let fresh = super::lone_learned(
            w.plane,
            NodeKind::Store,
            &name,
            Pool::Work,
            kinds::STORE_KEPT,
            super::store_cost_key(&checkout),
        );
        let node = fresh.first();
        let wanted = Arc::new(AtomicBool::new(false)); // alloc: once a store, shared with the task
        let does = Does::StoreKept {
            steps: Arc::clone(w.steps), // clone: a refcount, once a store
            kept: Lock::new("serve.job.store_members.kept", Some(front)),
            checkout: checkout.clone(), // clone: the task's span's name, once a store
            build: 0,
            logs: false,
            wanted: Arc::clone(&wanted), // clone: a refcount, once a store
            filing: None,
        };
        // The task is no job's: nothing cancels it but its own end.
        let never = Arc::new(AtomicBool::new(false)); // alloc: once a store
        let work = Step::of(&never, does);
        let command = "store the workspace members' pages of a kept front end";
        self.store_requests.insert(request);
        let posted = match w.plane.add(w.now, request, fresh, Some(work), false) {
            Ok(()) => {
                self.roots.storing[root as usize] += 1;
                self.fronts.root(root).members = Members::Writing;
                // alloc: once a store
                self.settling.push(super::Settling {
                    job: members_job(root),
                    command: command.to_owned(),
                    checkout: Some(checkout.clone()), // clone: the checkout's path, once a store
                    view: checkout.clone(), // clone: as above
                    storing: Some(super::Storing {
                        job: members_job(root),
                        command: command.to_owned(),
                        request,
                        node,
                        root,
                        checkout,
                        wanted,
                        promoted: false,
                        value: None,
                    }),
                    stored: blaze_cas::Tickets::default(),
                    durable: blaze_cas::Tickets::default(),
                    layer: blaze_cas::Tickets::default(),
                    layer_since: None,
                    layer_wait: None,
                    placing: 0,
                    placing_span: None,
                    leases: None,
                    since: None,
                    wait: None,
                });
                true
            }
            Err(error) => {
                crate::serve::said::logged!(WARN,
                    "blaze serve: the plane refused the task storing the workspace members' pages of the kept front end of \
                     {checkout}, so the front end is dropped with it and its next build lowers anew: {error}"
                );
                false
            }
        };
        w.plane.close(w.now, request);
        posted
    }

    /// At a stop: the members of every kept front end not written yet are written
    /// ([`Jobs::store_members`]), and the stop waits for them as for what answered builds left
    /// to settle (`Daemon::stop`). How many stores were posted.
    pub fn store_all_members(&mut self, w: &mut World<'_, S>) -> usize {
        let mut posted = 0;
        for at in 0..self.roots.fronts.len() {
            let root = at as u32;
            let members = self.fronts.roots.get(at).map_or(Members::Unwritten, |it| it.members);
            if self.roots.fronts[at].is_some()
                && members == Members::Unwritten
                && self.store_members(w, root)
            {
                posted += 1;
            }
        }
        posted
    }
}

/// Once an idle stretch, after [`idle::AFTER`] with no job: the caches that cost nothing to make
/// again go (`serve::idle`'s rule 1), blaze-mem's warm chunks and the flight reader's past
/// names, each under its own span with what it held before and after. What the next edit makes
/// again stays.
fn trim_idle() {
    let _span = tracing::info_span!(target: "blaze", "idle: caches that cost nothing to make again").entered();
    {
        let span = tracing::info_span!(target: "blaze", "chunks", before = tracing::field::Empty, after = tracing::field::Empty);
        let _entered = span.enter();
        let before = blaze_mem::chunks::cached().warm;
        let chunks = blaze_mem::chunks::trim();
        tracing::debug!(target: "blaze", chunks, "warm chunks trimmed");
        span.record("before", before);
        span.record("after", blaze_mem::chunks::cached().warm);
    }
    let span = tracing::info_span!(target: "blaze", "flight names", before = tracing::field::Empty, after = tracing::field::Empty);
    let _entered = span.enter();
    let before = blaze_profile::flight::names_held();
    let trimmed = blaze_profile::flight::trim_names();
    tracing::debug!(target: "blaze", trimmed, "flight names trimmed");
    span.record("before", before);
    span.record("after", blaze_profile::flight::names_held());
}
