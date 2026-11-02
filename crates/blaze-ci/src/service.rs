//! What each event makes happen (docs/v2/plan/lanes/ci.md section 10), as a state machine with no IO, no
//! clock and no threads: [`Service::step`] takes an [`Input`] and the time and says what to do
//! in [`Output`]s. [`crate::runtime`] does it.
//!
//! The service is a client of the warm daemon's agent API. It never writes the store (only the
//! daemon does), never starts a build itself and never hands the daemon a secret: each check is
//! one request on a view.
//!
//! - **A checkout is a view.** A commit's tree becomes a root on the daemon by the smallest diff
//!   at hand: the ref's last checkout, else (for a pull request) a checkout of its base commit,
//!   else the default branch's, else the empty tree. The ops of the git diff are applied to that
//!   root as a what-if and the result is held open ([`Output::Pin`]) so the daemon's GC keeps
//!   it until the ref's next checkout. One checkout of a ref runs at a time, and each one that
//!   ends becomes the ref's checkout, so the default branch's head `ci/<branch>` (committed with
//!   the same ops, to survive a daemon restart) always moves from the tree it is at.
//! - **Checks**: each configured check is one check run, posted queued at once, in progress when
//!   its request is sent and completed at its end, on the commit the event named.
//! - **Latest wins**: a newer push to a branch or pull request cancels the older one's requests
//!   and posts its check runs cancelled. Tags go through the [`Train`]: one release at a time,
//!   a newer tag replacing the waiting one.
//! - **Fork pull requests** run as any other: the daemon sandboxes every build script, proc macro
//!   and test, and the service adds no secret to any request (the installation token only
//!   fetches the commit into the VM's clone).
//! - **The oracle** ([`crate::oracle`]): a sampled view is also verified from nothing.
//! - **Snapshots** ([`crate::snapshot`]): after a green run of the default branch or a release.

#[cfg(test)]
mod tests;

use std::{
    collections::BTreeMap,
    fmt::{self, Write as _},
    ops::Range,
};

use blaze_api::{Diagnostic, Level, Mismatch, Request, Response, TestOutcome, TestResult, ViewRef};
use blaze_hash::{HashMap, HashSet};
use blaze_ids::ContentId;
use blaze_vfs::View;
use serde::{Deserialize, Serialize};

use crate::{
    CheckRun, CheckStatus, Checkout, Event, Train,
    checkout::pull_ref,
    config::{Check, CheckKind, Config},
    oracle,
    snapshot::{self, Kept},
};

/// How many checkouts of branches and pull requests are held open besides the default
/// branch's; past it, the one used longest ago is let go. A pull request's close is no event,
/// so without a bound every root ever checked out would be held.
pub const MOST_CHECKOUTS: usize = 256;

/// The most errors and failed tests a check run's summary lists; the rest are counted.
const MOST_LISTED: usize = 100;

/// The most of a failed test's output a summary shows: its last lines, then its last bytes.
const TAIL_LINES: usize = 30;
const TAIL_BYTES: usize = 4096;

/// What of [`Config`] the service decides with.
#[derive(PartialEq, Eq, Debug)]
pub struct Settings {
    pub default_branch: String,
    pub checks: Vec<Check>,
    pub release_checks: Vec<Check>,
    pub oracle_one_in: u64,
    pub oracle_targets: blaze_api::Targets,
    pub snapshot_every_secs: u64,
    pub snapshot_keep: usize,
}

impl Settings {
    pub fn of(config: &Config) -> Settings {
        Settings {
            default_branch: config.default_branch.clone(), // clone: once, at start
            checks: config.checks.clone(),                 // clone: once, at start
            release_checks: config.release_checks.clone(), // clone: once, at start
            oracle_one_in: config.oracle.one_in,
            oracle_targets: config.oracle.targets.to_targets(),
            snapshot_every_secs: config.snapshot.every_secs,
            snapshot_keep: config.snapshot.keep,
        }
    }
}

/// What the service keeps across restarts, in its state file.
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct State {
    /// By ref: `branch/<b>`, `pr/<n>`.
    // BTreeMap: serialized with serde as a map into the state file.
    pub checkouts: BTreeMap<String, Held>,
    pub snapshots: Vec<Kept>,
}

/// A ref's last checkout, held open on the daemon.
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct Held {
    pub checkout: Checkout,
    /// When it was last made or used as a base, in seconds since the epoch.
    pub at: u64,
}

/// A job: what one event runs, on one commit.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct JobId(pub u64);

/// A check run, as the service names it before GitHub has given it an id.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RunId(pub u64);

#[derive(PartialEq, Eq, Debug)]
pub enum Input {
    /// A delivery whose signature checked.
    Event(Event),
    /// The job's commit is a root on the daemon, held open once; or why not.
    CheckedOut { job: JobId, result: Result<ContentId, String> },
    /// A response of the request sent for `run`, before its `Done`.
    Response { job: JobId, run: RunId, response: Response },
    /// The request sent for `run` is over: its `Done`, or `ok` false when the daemon could not
    /// be asked (after a [`Response::Error`] saying why).
    Ended { job: JobId, run: RunId, ok: bool },
    /// GitHub created the check run, with its id; or why not, after retries.
    Created { run: RunId, result: Result<u64, String> },
    /// A root held open was lost (the daemon restarted without it).
    Unpinned { root: ContentId, why: String },
    Snapshotted(Result<Kept, String>),
    Removed { id: String, result: Result<(), String> },
}

#[derive(PartialEq, Debug)]
pub enum Output {
    /// Hold `root` open once more on the pin connection.
    Pin { root: ContentId },
    /// Let go of one hold of `root`.
    Unpin { root: ContentId },
    /// Fetch `fetch`, diff `from`'s commit (the empty tree for `None`) to `commit`, apply the ops
    /// to `from`'s root (`Head("ci/empty")` for `None`) and hold the result open; with `head`,
    /// commit the same ops to that head and check it reached the same root. Answered with
    /// [`Input::CheckedOut`]. The job's worker starts here.
    Checkout { job: JobId, commit: String, fetch: Vec<String>, from: Option<Checkout>, head: Option<String> },
    /// Send `request` for `run`, on the job's own connection.
    Send { job: JobId, run: RunId, request: Request },
    /// Cancel every request of the job still running; each still ends with [`Input::Ended`].
    Cancel { job: JobId },
    /// The job's worker has nothing more to do.
    EndJob { job: JobId },
    /// Create the check run; answered with [`Input::Created`].
    Create { run: RunId, job: JobId, check: CheckRun },
    Update { run: RunId, id: u64, check: CheckRun },
    /// Write the state file, [`Service::state`]: once for any number of these together.
    Save,
    /// [`Request::Snapshot`], then the snapshot command with `name`; answered with
    /// [`Input::Snapshotted`].
    Snapshot { name: String },
    /// The remove command for `id`; answered with [`Input::Removed`].
    RemoveSnapshot { id: String },
    /// A line for stderr: an error no check run carries.
    Log(String),
}

#[derive(PartialEq, Eq, Debug)]
enum JobKind {
    Branch { default: bool },
    Pull { number: u64, base: String },
    Tag { name: String },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    /// Waiting for the ref's checkout in flight, or for the train.
    Queued,
    CheckingOut,
    Running,
}

#[derive(Debug)]
struct Job {
    reference: String,
    commit: String,
    kind: JobKind,
    phase: Phase,
    /// Its check runs, made together: ids `first..first + count`, so walking them clones nothing.
    first: u64,
    count: u32,
    /// A newer event replaced it: its check runs are posted cancelled.
    replaced: bool,
    /// How many of its runs have a request sent and not ended ([`Run::pending`]).
    pending: u32,
    /// [`Output::Checkout`] was given: the runtime has a worker to end.
    worker: bool,
    root: Option<ContentId>,
    /// The root is held for the job alone (a tag's), let go when it ends.
    owns_root: bool,
    /// The base of its checkout, held while the checkout runs.
    base: Option<ContentId>,
}

impl Job {
    /// The ids of its runs, as numbers: `.map(RunId)` them.
    fn runs(&self) -> Range<u64> {
        self.first..self.first + u64::from(self.count)
    }
}

/// What a run asks the daemon: 8 bytes, copied, not a clone of its [`Check`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Work {
    /// The check at `at` in [`Settings::checks`], or in [`Settings::release_checks`] when
    /// `release`.
    Check { kind: CheckKind, release: bool, at: u32 },
    Oracle,
}

const _: () = assert!(size_of::<Work>() == 8);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GitHubId {
    Creating,
    Id(u64),
    /// Creating it failed; the runtime logged it, and nothing more is posted.
    Failed,
}

#[derive(Debug)]
struct Run {
    job: JobId,
    check: CheckRun,
    work: Work,
    github: GitHubId,
    /// `check` changed while its creation waits for an id: sent once the id comes. Once it has
    /// one, each change is sent as it is made, so what GitHub has is `check`.
    unposted: bool,
    /// Its request is sent and not ended.
    pending: bool,
    report: Report,
    /// Completed: nothing changes it again.
    done: bool,
}

/// No map here is iterated, so none decides an order: each is a hash table on an id, two
/// multiply-and-shift hashes and no string compare per lookup.
pub struct Service {
    settings: Settings,
    /// `branch/<default>`, the default branch's ref, made once.
    default_ref: String,
    state: State,
    now: u64,
    jobs: HashMap<JobId, Job>,
    runs: HashMap<RunId, Run>,
    /// Each branch's and pull request's newest job.
    latest: HashMap<String, JobId>,
    /// The job whose checkout of a ref is in flight.
    checking_out: HashMap<String, JobId>,
    train: Train,
    release_running: Option<JobId>,
    release_waiting: Option<JobId>,
    snapshotting: bool,
    removing: HashSet<String>,
    next_job: u64,
    next_run: u64,
}

impl Service {
    /// The service as `state` left it: each checkout it kept is held open again.
    pub fn new(settings: Settings, state: State, out: &mut Vec<Output>) -> Service {
        for held in state.checkouts.values() {
            out.push(Output::Pin { root: held.checkout.root });
        }
        Service {
            default_ref: format!("branch/{}", settings.default_branch), // alloc: once, at start
            settings,
            state,
            now: 0,
            jobs: HashMap::default(),
            runs: HashMap::default(),
            latest: HashMap::default(),
            checking_out: HashMap::default(),
            train: Train::default(),
            release_running: None,
            release_waiting: None,
            snapshotting: false,
            removing: HashSet::default(),
            next_job: 0,
            next_run: 0,
        }
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    /// Jobs not yet over.
    pub fn jobs(&self) -> usize {
        self.jobs.len()
    }

    /// `input`, arrived at `now` (seconds since the epoch).
    pub fn step(&mut self, now: u64, input: Input, out: &mut Vec<Output>) {
        self.now = now;
        match input {
            Input::Event(event) => self.event(event, out),
            Input::CheckedOut { job, result } => self.checked_out(job, result, out),
            Input::Response { job, run, response } => self.response(job, run, response),
            Input::Ended { job, run, ok } => self.ended(job, run, ok, out),
            Input::Created { run, result } => self.created(run, result, out),
            Input::Unpinned { root, why } => self.unpinned(root, &why, out),
            Input::Snapshotted(result) => self.snapshotted(result, out),
            Input::Removed { id, result } => self.removed(id, result, out),
        }
    }

    fn event(&mut self, event: Event, out: &mut Vec<Output>) {
        match event {
            Event::Push { branch, commit } => {
                let default = branch == self.settings.default_branch;
                // alloc: once per event, the new job's ref
                self.arrive(format!("branch/{branch}"), commit, JobKind::Branch { default }, out);
            }
            Event::PullRequest { number, head, base, from_fork: _ } => {
                // alloc: once per event, the new job's ref
                self.arrive(format!("pr/{number}"), head, JobKind::Pull { number, base }, out);
            }
            Event::Tag { name, commit } => self.tag(name, commit, out),
        }
    }

    /// A push to a branch or pull request: the newest wins.
    fn arrive(&mut self, reference: String, commit: String, kind: JobKind, out: &mut Vec<Output>) {
        if let Some(old) = self.latest.get(&reference).copied() {
            // alloc: once per replaced job, the summary its check runs are posted with
            self.replace(old, &format!("Cancelled: replaced by {commit}."), out);
        }
        // clone: once per event
        let job = self.new_job(reference.clone(), commit, kind, false, out);
        self.latest.insert(reference, job);
        self.try_start(job, out);
    }

    fn tag(&mut self, name: String, commit: String, out: &mut Vec<Output>) {
        // alloc: clone: once per tag, the job's ref and its kind's name
        let job = self.new_job(format!("tag/{name}"), commit, JobKind::Tag { name: name.clone() }, true, out);
        let idle = self.train.running.is_none();
        let replaced = self.train.arrive(name.clone()); // clone: once per tag, the train's
        if idle {
            self.release_running = Some(job);
            self.try_start(job, out);
            return;
        }
        let old = self.release_waiting.replace(job);
        if let (Some(old), Some(_)) = (old, replaced) {
            // alloc: once per replaced tag, the summary its check runs are posted with
            self.replace(old, &format!("Cancelled: replaced by {name}."), out);
        }
        let running = self.train.running.as_deref().unwrap_or_default();
        // alloc: once per tag waiting
        let summary = format!("Queued: waiting for release {running} to finish.");
        for run in self.jobs[&job].runs().map(RunId) {
            // clone: once per check-run post
            self.set(run, CheckStatus::Queued, summary.clone(), out);
        }
    }

    /// A job running each of the configured checks (the release checks when `release`), and the
    /// oracle when the commit is sampled: its runs posted queued.
    fn new_job(
        &mut self,
        reference: String,
        commit: String,
        kind: JobKind,
        release: bool,
        out: &mut Vec<Output>,
    ) -> JobId {
        let job = JobId(self.next_job);
        self.next_job += 1;
        let checks = if release { &self.settings.release_checks } else { &self.settings.checks };
        let sampled = oracle::sampled(&commit, self.settings.oracle_one_in);
        let count = checks.len() + usize::from(sampled);
        let first = self.next_run;
        self.next_run += count as u64;
        // alloc: once per job
        let queued = format!("Queued: waiting for the checkout of {commit}.");
        self.runs.reserve(count);
        for at in 0..count {
            let run = RunId(first + at as u64);
            let (name, work) = match checks.get(at) {
                Some(check) => {
                    // clone: once per check run, its name as GitHub shows it
                    (check.name.clone(), Work::Check { kind: check.kind, release, at: at as u32 })
                }
                None => (oracle::CHECK.to_owned(), Work::Oracle), // alloc: once per oracle run
            };
            let check =
                // clone: once per check run: what GitHub is posted, and the run's own copy
                CheckRun { name, commit: commit.clone(), status: CheckStatus::Queued, summary: queued.clone() };
            // clone: once per check run created
            out.push(Output::Create { run, job, check: check.clone() });
            let github = GitHubId::Creating;
            let report = Report::default();
            let run_of = Run { job, check, work, github, unposted: false, pending: false, report, done: false };
            self.runs.insert(run, run_of);
        }
        self.jobs.insert(
            job,
            Job {
                reference,
                commit,
                kind,
                phase: Phase::Queued,
                first,
                count: count as u32,
                replaced: false,
                pending: 0,
                worker: false,
                root: None,
                owns_root: false,
                base: None,
            },
        );
        job
    }

    /// A newer event replaced `job`: its check runs are posted cancelled and its requests
    /// cancelled.
    fn replace(&mut self, job: JobId, summary: &str, out: &mut Vec<Output>) {
        let Some(it) = self.jobs.get_mut(&job) else { return };
        it.replaced = true;
        let phase = it.phase;
        let pending = it.pending > 0;
        for run in it.runs().map(RunId) {
            // alloc: once per check-run post
            self.conclude(run, CheckStatus::Cancelled, summary.to_owned(), out);
        }
        match phase {
            Phase::Queued => self.finish(job, out),
            Phase::CheckingOut => {}
            Phase::Running if pending => out.push(Output::Cancel { job }),
            Phase::Running => self.finish(job, out),
        }
    }

    /// Where the job's checkout diffs from: the nearest tree already held.
    ///
    /// Once per checkout: the copy goes to the job's worker in [`Output::Checkout`].
    fn from(&self, job: &Job) -> Option<Checkout> {
        // clone: once per checkout
        let held = |reference: &str| self.state.checkouts.get(reference).map(|it| it.checkout.clone());
        if job.kind == (JobKind::Branch { default: true }) {
            // The head `ci/<branch>` is at this ref's checkout; its commit moves from there.
            return held(&job.reference);
        }
        let with_commit = |commit: &str| {
            // clone: once per checkout
            self.state.checkouts.values().find(|it| it.checkout.commit == commit).map(|it| it.checkout.clone())
        };
        if let Some(same) = with_commit(&job.commit) {
            return Some(same);
        }
        if let Some(own) = held(&job.reference) {
            return Some(own);
        }
        if let JobKind::Pull { base, .. } = &job.kind
            && let Some(base) = with_commit(base)
        {
            return Some(base);
        }
        held(&self.default_ref)
    }

    fn try_start(&mut self, job: JobId, out: &mut Vec<Output>) {
        let Some(it) = self.jobs.get(&job) else { return };
        if it.phase != Phase::Queued || self.checking_out.contains_key(&it.reference) {
            return;
        }
        if matches!(it.kind, JobKind::Tag { .. }) && self.release_running != Some(job) {
            return;
        }
        let from = self.from(it);
        // alloc: clone: once per checkout, what the job's worker is handed
        let mut fetch = Vec::with_capacity(2);
        if let JobKind::Pull { number, .. } = it.kind {
            fetch.push(pull_ref(number));
        }
        fetch.push(it.commit.clone()); // clone: once per checkout
        let head = match it.kind {
            // alloc: once per checkout
            JobKind::Branch { default: true } => Some(format!("ci/{}", self.settings.default_branch)),
            _ => None,
        };
        let commit = it.commit.clone(); // clone: once per checkout
        // clone: once per checkout, the key of the one in flight
        let reference = it.reference.clone();
        if let Some(from) = &from {
            out.push(Output::Pin { root: from.root });
            let now = self.now;
            if let Some(held) = self.state.checkouts.values_mut().find(|it| it.checkout == *from) {
                held.at = now;
            }
        }
        let it = self.jobs.get_mut(&job).expect("the job is there");
        it.base = from.as_ref().map(|from| from.root);
        it.phase = Phase::CheckingOut;
        it.worker = true;
        self.checking_out.insert(reference, job);
        out.push(Output::Checkout { job, commit, fetch, from, head });
    }

    fn checked_out(&mut self, job: JobId, result: Result<ContentId, String>, out: &mut Vec<Output>) {
        let Some(it) = self.jobs.get_mut(&job) else { return };
        if it.phase != Phase::CheckingOut {
            return;
        }
        // clone: once per checkout: the job may end below, and its ref picks the next
        let reference = it.reference.clone();
        if self.checking_out.get(&reference) == Some(&job) {
            self.checking_out.remove(&reference);
        }
        if let Some(base) = it.base.take() {
            out.push(Output::Unpin { root: base });
        }
        match result {
            Ok(root) => {
                it.root = Some(root);
                if matches!(it.kind, JobKind::Tag { .. }) {
                    it.owns_root = true;
                } else {
                    // clone: once per checkout kept
                    let checkout = Checkout { commit: it.commit.clone(), root };
                    self.keep(&reference, checkout, out);
                }
                let it = self.jobs.get_mut(&job).expect("the job is there");
                if it.replaced {
                    self.finish(job, out);
                } else {
                    self.run(job, root, out);
                }
            }
            Err(what) => {
                if !it.replaced {
                    // alloc: error path
                    let summary = format!("The checkout of {} failed, so nothing ran: {what}", it.commit);
                    for run in it.runs().map(RunId) {
                        // clone: error path
                        self.conclude(run, CheckStatus::Failure, summary.clone(), out);
                    }
                }
                self.finish(job, out);
            }
        }
        if let Some(next) = self.latest.get(&reference).copied()
            && next != job
        {
            self.try_start(next, out);
        }
    }

    /// `checkout` becomes `reference`'s, held in place of the one it had.
    fn keep(&mut self, reference: &str, checkout: Checkout, out: &mut Vec<Output>) {
        let held = Held { checkout, at: self.now };
        match self.state.checkouts.get_mut(reference) {
            Some(kept) => {
                let old = std::mem::replace(kept, held);
                out.push(Output::Unpin { root: old.checkout.root });
            }
            None => {
                // alloc: a ref's first checkout
                self.state.checkouts.insert(reference.to_owned(), held);
            }
        }
        let checkouts = &mut self.state.checkouts;
        let default_ref = &self.default_ref;
        let mut others = checkouts.len() - usize::from(checkouts.contains_key(default_ref));
        while others > MOST_CHECKOUTS {
            let oldest = checkouts
                .iter()
                .filter(|(reference, _)| *reference != default_ref)
                .min_by_key(|&(reference, held)| (held.at, reference))
                // clone: once per checkout let go past the most held
                .map(|(reference, _)| reference.clone())
                .expect("more than none");
            let held = checkouts.remove(&oldest).expect("it is there");
            out.push(Output::Unpin { root: held.checkout.root });
            others -= 1;
        }
        out.push(Output::Save);
    }

    /// Sends each check's request on the job's view.
    fn run(&mut self, job: JobId, root: ContentId, out: &mut Vec<Output>) {
        let it = self.jobs.get_mut(&job).expect("the job is there");
        it.phase = Phase::Running;
        let runs = it.runs();
        let view = || ViewRef::Root(View { root });
        let mut pending = 0;
        for run in runs.map(RunId) {
            let Some(it) = self.runs.get_mut(&run) else { continue };
            if it.done {
                continue;
            }
            let (request, what) = match it.work {
                Work::Check { kind, release, at } => {
                    let checks = if release { &self.settings.release_checks } else { &self.settings.checks };
                    (checks[at as usize].request(view()), kind_name(kind))
                }
                Work::Oracle => (
                    // clone: once per oracle run, handed to its request
                    Request::Verify { view: view(), targets: self.settings.oracle_targets.clone() },
                    "verify",
                ),
            };
            it.pending = true;
            pending += 1;
            out.push(Output::Send { job, run, request });
            let commit = &self.jobs[&job].commit;
            // alloc: once per check-run post
            let summary = format!("Running `{what}` on the tree of {commit} (root {root:x}).");
            self.set(run, CheckStatus::InProgress, summary, out);
        }
        self.jobs.get_mut(&job).expect("the job is there").pending = pending;
        if pending == 0 {
            self.finish(job, out);
        }
    }

    fn response(&mut self, job: JobId, run: RunId, response: Response) {
        if let Some(it) = self.runs.get_mut(&run)
            && it.job == job
            && it.pending
            && !it.done
        {
            it.report.add(response);
        }
    }

    fn ended(&mut self, job: JobId, run: RunId, ok: bool, out: &mut Vec<Output>) {
        let (Some(it), Some(of)) = (self.runs.get_mut(&run), self.jobs.get_mut(&job)) else { return };
        if it.job != job || !it.pending {
            return;
        }
        it.pending = false;
        of.pending -= 1;
        let over = of.pending == 0 && of.phase == Phase::Running;
        if !it.done {
            let (status, summary) = it.outcome(ok);
            self.conclude(run, status, summary, out);
        }
        if over {
            self.finish(job, out);
        }
    }

    /// The job is over: its worker ends, its root is let go if it owned it, the train moves, and
    /// a green run of the default branch or a release may snapshot the VM.
    fn finish(&mut self, job: JobId, out: &mut Vec<Output>) {
        let Some(it) = self.jobs.remove(&job) else { return };
        if self.latest.get(&it.reference) == Some(&job) {
            self.latest.remove(&it.reference);
        }
        if self.checking_out.get(&it.reference) == Some(&job) {
            self.checking_out.remove(&it.reference);
        }
        if it.worker {
            out.push(Output::EndJob { job });
        }
        if it.owns_root
            && let Some(root) = it.root
        {
            out.push(Output::Unpin { root });
        }
        for run in it.runs().map(RunId) {
            let Some(of) = self.runs.get_mut(&run) else { continue };
            of.pending = false;
            if !of.done {
                // alloc: once per check-run post
                let summary = "The job ended without a result.".to_owned();
                self.conclude(run, CheckStatus::Failure, summary, out);
            }
        }
        let green = !it.replaced
            && it.runs().map(RunId).all(|run| {
                self.runs.get(&run).is_some_and(|it| it.check.status == CheckStatus::Success)
            });
        for run in it.runs().map(RunId) {
            self.collect(run);
        }
        if self.release_waiting == Some(job) {
            self.release_waiting = None;
        }
        if self.release_running == Some(job) {
            self.release_running = None;
            if self.train.finish().is_some()
                && let Some(next) = self.release_waiting.take()
            {
                self.release_running = Some(next);
                self.try_start(next, out);
            }
        }
        let snapshots = matches!(it.kind, JobKind::Branch { default: true } | JobKind::Tag { .. });
        if green && snapshots {
            self.maybe_snapshot(&it.commit, out);
        }
    }

    /// Forgets a run once its job is over and GitHub has answered its creation.
    fn collect(&mut self, run: RunId) {
        let Some(it) = self.runs.get(&run) else { return };
        if it.github != GitHubId::Creating && !self.jobs.contains_key(&it.job) {
            self.runs.remove(&run);
        }
    }

    /// Completes the run, unless it is already.
    fn conclude(&mut self, run: RunId, status: CheckStatus, summary: String, out: &mut Vec<Output>) {
        let Some(it) = self.runs.get_mut(&run) else { return };
        if it.done {
            return;
        }
        it.done = true;
        self.set(run, status, summary, out);
    }

    /// The run's status and summary; sent to GitHub when they changed, or kept to be sent once
    /// the run has an id. A change back to what was sent can't come while it waits: the only
    /// summary it was created with is never made again.
    fn set(&mut self, run: RunId, status: CheckStatus, summary: String, out: &mut Vec<Output>) {
        let Some(it) = self.runs.get_mut(&run) else { return };
        if it.check.status == status && it.check.summary == summary {
            return;
        }
        it.check.status = status;
        it.check.summary = summary;
        match it.github {
            // clone: once per check-run post, handed to the GitHub thread
            GitHubId::Id(id) => out.push(Output::Update { run, id, check: it.check.clone() }),
            GitHubId::Creating => it.unposted = true,
            GitHubId::Failed => {}
        }
    }

    fn created(&mut self, run: RunId, result: Result<u64, String>, out: &mut Vec<Output>) {
        let Some(it) = self.runs.get_mut(&run) else { return };
        if it.github != GitHubId::Creating {
            return;
        }
        match result {
            Ok(id) => {
                it.github = GitHubId::Id(id);
                if std::mem::take(&mut it.unposted) {
                    // clone: once per check-run post
                    out.push(Output::Update { run, id, check: it.check.clone() });
                }
            }
            Err(what) => {
                it.github = GitHubId::Failed;
                // alloc: error path
                out.push(Output::Log(format!(
                    "check run {} on {} was never created, so its result is not on GitHub: {what}",
                    it.check.name, it.check.commit
                )));
            }
        }
        self.collect(run);
    }

    /// One pass over the checkouts, the lost ones' refs written straight into the line logged.
    fn unpinned(&mut self, root: ContentId, why: &str, out: &mut Vec<Output>) {
        let mut line = String::new(); // alloc: error path, the daemon lost a root
        let mut lost = 0;
        self.state.checkouts.retain(|reference, held| {
            if held.checkout.root != root {
                return true;
            }
            if lost == 0 {
                let _ = write!(line, "the daemon no longer holds root {root:x}, the checkout of ");
            } else {
                line.push_str(", ");
            }
            line.push_str(reference);
            lost += 1;
            false
        });
        if lost == 0 {
            return;
        }
        let _ = write!(line, ": {why}; their next checkout diffs from another tree");
        out.push(Output::Log(line));
        out.push(Output::Save);
    }

    fn maybe_snapshot(&mut self, commit: &str, out: &mut Vec<Output>) {
        if self.snapshotting
            || !snapshot::due(&self.state.snapshots, self.now, self.settings.snapshot_every_secs)
        {
            return;
        }
        self.snapshotting = true;
        out.push(Output::Snapshot { name: snapshot::name(commit, self.now) });
    }

    fn snapshotted(&mut self, result: Result<Kept, String>, out: &mut Vec<Output>) {
        self.snapshotting = false;
        match result {
            Ok(kept) => {
                self.state.snapshots.push(kept);
                out.push(Output::Save);
                for id in snapshot::beyond(&self.state.snapshots, self.settings.snapshot_keep) {
                    // clone: once per snapshot removed
                    if self.removing.insert(id.clone()) {
                        out.push(Output::RemoveSnapshot { id });
                    }
                }
            }
            // alloc: error path
            Err(what) => out.push(Output::Log(format!(
                "the snapshot failed, and the next green run tries again: {what}"
            ))),
        }
    }

    fn removed(&mut self, id: String, result: Result<(), String>, out: &mut Vec<Output>) {
        self.removing.remove(&id);
        match result {
            Ok(()) => {
                self.state.snapshots.retain(|it| it.id != id);
                out.push(Output::Save);
            }
            // alloc: error path
            Err(what) => out.push(Output::Log(format!(
                "removing snapshot {id} failed, and it is tried again after the next snapshot: {what}"
            ))),
        }
    }
}

fn kind_name(kind: CheckKind) -> &'static str {
    match kind {
        CheckKind::Check => "check",
        CheckKind::Build => "build",
        CheckKind::Test => "test",
    }
}

impl Run {
    /// The run's status and summary once its request ended with `ok`.
    fn outcome(&self, ok: bool) -> (CheckStatus, String) {
        let report = &self.report;
        match self.work {
            Work::Oracle => match &report.verified {
                Some(mismatches) if ok && mismatches.is_empty() => {
                    (CheckStatus::Success, oracle::summary(mismatches))
                }
                Some(mismatches) if !mismatches.is_empty() => {
                    (CheckStatus::Failure, report.with_errors(oracle::summary(mismatches)))
                }
                _ => (
                    CheckStatus::Failure,
                    // alloc: once per check-run post, a verification that said nothing
                    report.with_errors("The verification ended without a result.".to_owned()),
                ),
            },
            Work::Check { kind, .. } => {
                let green = ok
                    && report.errors == 0
                    && report.failed == 0
                    && report.said.is_empty()
                    && report.unexpected.is_empty();
                let status = if green { CheckStatus::Success } else { CheckStatus::Failure };
                (status, report.summary(kind, ok))
            }
        }
    }
}

/// What a summary holds besides its lists: its first line, the lists' headers and the counts
/// of what they leave out.
const SUMMARY_ROOM: usize = 512;

/// What a request's responses said, kept for its check run's summary.
///
/// Each list is one string, every item written into it once as the summary shows it, so a
/// response costs no allocation of its own: a count, or a write into a buffer that grows by
/// doubling. Only the first [`MOST_LISTED`] errors, failed tests and unexpected responses are
/// written; the rest are counted.
#[derive(Default, Debug)]
struct Report {
    errors: usize,
    warnings: usize,
    /// Each error as `\n<error>\n`.
    listed: String,
    listed_count: usize,
    passed: usize,
    failed: usize,
    ignored: usize,
    unreached: usize,
    /// Each failed test as `\n<failure>\n`.
    failures: String,
    failures_count: usize,
    artifacts: usize,
    verified: Option<Vec<Mismatch>>,
    /// The daemon's errors, each as `\n- <error>`.
    said: String,
    /// Each response the request should not have had as `\n- <response>`, cut to 200 bytes.
    unexpected: String,
    unexpected_count: usize,
}

impl Report {
    fn add(&mut self, response: Response) {
        match response {
            Response::Diagnostic(diagnostic) => match diagnostic.level {
                Level::Error => {
                    self.errors += 1;
                    if self.listed_count < MOST_LISTED {
                        self.listed_count += 1;
                        let mut len = Len(2);
                        let _ = render_diagnostic_into(&mut len, &diagnostic);
                        self.listed.reserve(len.0);
                        self.listed.push('\n');
                        let _ = render_diagnostic_into(&mut self.listed, &diagnostic);
                        self.listed.push('\n');
                    }
                }
                Level::Warning => self.warnings += 1,
                Level::Note | Level::Help | Level::FailureNote => {}
            },
            Response::TestResult(test) => match test.outcome {
                TestOutcome::Passed => self.passed += 1,
                TestOutcome::Ignored => self.ignored += 1,
                TestOutcome::Unreached => self.unreached += 1,
                TestOutcome::Failed => {
                    self.failed += 1;
                    if self.failures_count < MOST_LISTED {
                        self.failures_count += 1;
                        self.failures.push('\n');
                        render_failure_into(&mut self.failures, &test);
                        self.failures.push('\n');
                    }
                }
            },
            Response::Built { artifacts, .. } => self.artifacts += artifacts.len(),
            Response::Unit(_)
            | Response::UnitEnded(_)
            | Response::Fetched { .. }
            | Response::BuildScript(_)
            | Response::Work(_) => {}
            Response::Verified { mismatches } => self.verified = Some(mismatches),
            Response::Error(error) => {
                self.said.push_str("\n- ");
                self.said.push_str(&error.what);
                if let Some(at) = &error.at {
                    let _ = write!(self.said, " (at {at})");
                }
            }
            other => {
                if self.unexpected_count < MOST_LISTED {
                    self.unexpected_count += 1;
                    self.unexpected.push_str("\n- ");
                    let start = self.unexpected.len();
                    let _ = write!(self.unexpected, "{other:?}");
                    cut(&mut self.unexpected, start, 200);
                }
            }
        }
    }

    /// The run's summary, written into one string sized for what it lists.
    fn summary(&self, kind: CheckKind, ok: bool) -> String {
        let lists = self.listed.len() + self.failures.len() + self.said.len() + self.unexpected.len();
        // alloc: once per check-run post, the summary GitHub is sent
        let mut text = String::with_capacity(SUMMARY_ROOM + lists);
        let _ = write!(
            text,
            "{} error{}, {} warning{}",
            self.errors,
            plural(self.errors),
            self.warnings,
            plural(self.warnings)
        );
        match kind {
            CheckKind::Test => {
                let _ = write!(
                    text,
                    "; tests: {} passed, {} failed, {} ignored, {} not reached by the change (their \
                     last results stand)",
                    self.passed, self.failed, self.ignored, self.unreached
                );
            }
            CheckKind::Build => {
                let _ = write!(text, "; {} artifacts built", self.artifacts);
            }
            CheckKind::Check => {}
        }
        text.push('.');
        if self.listed_count > 0 {
            text.push_str("\n\n## Errors\n");
            text.push_str(&self.listed);
            if self.errors > self.listed_count {
                let _ = write!(text, "\nand {} more errors\n", self.errors - self.listed_count);
            }
        }
        if self.failures_count > 0 {
            text.push_str("\n\n## Failed tests\n");
            text.push_str(&self.failures);
            if self.failed > self.failures_count {
                let _ = write!(text, "\nand {} more failed tests\n", self.failed - self.failures_count);
            }
        }
        let mut text = self.with_errors(text);
        let quiet = self.errors == 0 && self.failed == 0 && self.said.is_empty();
        if !ok && quiet {
            text.push_str("\n\nThe daemon ended the request as failed and said nothing more.");
        }
        text
    }

    /// `text`, then what the daemon said went wrong and any response the request should not
    /// have had.
    fn with_errors(&self, mut text: String) -> String {
        if !self.said.is_empty() {
            text.push_str("\n\n## The daemon's errors\n");
            text.push_str(&self.said);
            text.push('\n');
        }
        if !self.unexpected.is_empty() {
            text.push_str("\n\n## Responses this request should not have had\n");
            text.push_str(&self.unexpected);
            text.push('\n');
        }
        text
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// How many bytes a rendering writes, counted without writing them, so the list it goes in
/// grows once for it.
struct Len(usize);

impl fmt::Write for Len {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.0 += text.len();
        Ok(())
    }
}

/// An error as rustc prints its first line, its other labelled places, its notes and the facts
/// its query read, onto the end of `text`.
fn render_diagnostic_into(text: &mut impl fmt::Write, diagnostic: &Diagnostic) -> fmt::Result {
    text.write_str(diagnostic.level.name())?;
    if let Some(code) = &diagnostic.code {
        write!(text, "[{code}]")?;
    }
    match diagnostic.spans.first() {
        Some(span) => write!(text, " {}:{}:{}: ", span.file, span.line, span.column)?,
        None => text.write_str(" (no place): ")?,
    }
    text.write_str(&diagnostic.message)?;
    for span in &diagnostic.spans {
        if let Some(label) = &span.label {
            write!(text, "\n  {}:{}:{}: {label}", span.file, span.line, span.column)?;
        }
    }
    for child in &diagnostic.children {
        write!(text, "\n  {}: {}", child.level.name(), child.message)?;
    }
    for fact in &diagnostic.facts {
        text.write_str("\n  read: ")?;
        text.write_str(&fact.what)?;
    }
    Ok(())
}

/// A failed test, its name and the end of its output, onto the end of `text`.
fn render_failure_into(text: &mut String, test: &TestResult) {
    let _ = write!(
        text,
        "test {} `{}` failed after {} ms; the end of its output:\n```\n",
        test.binary, test.name, test.millis
    );
    tail_into(text, &test.output);
    text.push_str("\n```");
}

/// The last [`TAIL_LINES`] lines of `output`, at most [`TAIL_BYTES`] of them, with each run of
/// three backticks made three quotes, onto the end of `out`.
///
/// Only the end of the output is read: a test that printed megabytes costs what its tail does.
/// A suffix that starts just after a newline has the same lines as the whole, both decoded (a
/// newline ends any malformed UTF-8 sequence) and trimmed, so long as it has the lines asked
/// for; when trailing blank lines leave it short, a suffix twice as long is tried. The lines
/// are written once, into room reserved for them, and cut and escaped in place.
fn tail_into(out: &mut String, output: &[u8]) {
    let mut newlines = TAIL_LINES + 1;
    let text = loop {
        let start = output
            .iter()
            .rposition({
                let mut seen = 0;
                move |&byte| {
                    seen += usize::from(byte == b'\n');
                    seen == newlines
                }
            })
            .map_or(0, |at| at + 1);
        // alloc: only output that is not UTF-8 is copied; the rest is borrowed
        let text = String::from_utf8_lossy(&output[start..]);
        if start == 0 || text.trim_end().lines().count() >= TAIL_LINES {
            break text;
        }
        newlines *= 2;
    };
    let trimmed = text.trim_end();
    let skip = trimmed.lines().count().saturating_sub(TAIL_LINES);
    let lines = || trimmed.lines().skip(skip);
    let len = lines().map(|line| line.len() + 1).sum::<usize>().saturating_sub(1);
    let base = out.len();
    out.reserve(len + 3);
    for (at, line) in lines().enumerate() {
        if at > 0 {
            out.push('\n');
        }
        out.push_str(line);
    }
    if len > TAIL_BYTES {
        let mut start = out.len() - TAIL_BYTES;
        while !out.is_char_boundary(start) {
            start += 1;
        }
        out.replace_range(base..start, "...");
    }
    let mut from = base;
    while let Some(at) = out[from..].find("```") {
        let at = from + at;
        out.replace_range(at..at + 3, "'''");
        from = at + 3;
    }
}

/// `text` past `from` cut to at most `most` bytes, at a char boundary.
fn cut(text: &mut String, from: usize, most: usize) {
    if text.len() - from > most {
        let mut end = from + most;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("...");
    }
}
