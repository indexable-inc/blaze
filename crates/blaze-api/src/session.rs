//! What each request does (docs/v2/plan/lanes/ci.md, #277): a state machine over the VFS, the database and
//! the [`Builder`], with no IO, clock or threads.
//!
//! Every request ends with exactly one [`Response::Done`], after a [`Response::Error`] naming
//! what and where when it failed. A request that needs the build runs as jobs ([`JobId`], one
//! per candidate of a best-of-N) whose reports are forwarded under the request's id; the views a
//! job builds stay open in both the VFS and the database until it finishes, and the database's
//! view of the last incremental build of each of the latest [`KEPT`] roots stays open after,
//! for [`Request::Why`]. A commit is answered once its ticket is durable. A connection that is
//! gone has its requests cancelled and its views closed, and nothing more is sent to it.
//!
//! The daemon flushes the VFS ([`blaze_vfs::Vfs::flush`]) after each call, so what a call
//! committed is on its way to durability when the call returns.
//!
//! Declared intent ([`Request::WillTest`], [`Request::WillSave`]) is answered at once and
//! built ahead as jobs of no request ([`intent`]); the daemon gives the session its clock
//! through [`Session::tick`], before each other call and at [`Session::deadline`].

mod best_of;
mod intent;
#[cfg(test)]
mod tests;

use std::collections::VecDeque;

use blaze_core::Conn;
use blaze_hash::HashMap;
use blaze_ids::{ContentId, radix};
use blaze_mem::{
    Handle, Slab, Tag,
    tag::{Rate, Snapshot},
};
use blaze_query::QueryKey;
use blaze_vfs::{CommitError, OpError, Ticket, View, ViewId, Vfs};

use crate::{
    ApiError, Arrived, Memory, Reply, Request, RequestRef, Response, Status, TagMemory, Targets,
    ViewRef,
    build::{Builder, JobId, Opened, Work},
    session::{best_of::BestOf, intent::Intents},
};

/// What the session works on, lent for one call.
pub struct Daemon<'a> {
    pub vfs: &'a mut Vfs,
    pub db: &'a blaze_query::Database,
    // dyn: the daemon's one builder, chosen at startup; one call per job.
    pub builder: &'a mut dyn Builder,
    /// For [`Response::Status`].
    pub pid: u32,
    pub build: &'a crate::Build,
    pub memory_bytes: u64,
    pub budget_bytes: u64,
    /// blaze-mem's counters as they are now, for [`Response::Memory`]:
    /// `blaze_mem::tag::snapshot` in the daemon, read only when asked.
    pub memory: fn() -> Snapshot,
    /// The flight recorder's Chrome trace of the last given time, for [`Response::Profiled`]:
    /// `blaze_profile::flight::dump` in the daemon.
    pub profile: fn(std::time::Duration) -> Vec<u8>,
    /// The live sketches' top keys and wait quantiles, for [`Response::Hot`]: the daemon's
    /// reading of `blaze_profile::hot::read`.
    pub hot: fn(u32, crate::HotSince) -> Result<crate::Hot, ApiError>,
    /// The profile history's last error, for [`Status::history`].
    pub history: Option<&'a str>,
}

/// The requests under way, the views each connection holds, and the jobs of the build.
///
/// Each call adds to `out` the replies it makes, in order. The daemon flushes the VFS after
/// each call.
pub struct Session {
    next_serial: u64,
    /// Every request not yet over, by the session's own number for it; a gone connection's
    /// stay until their jobs finish.
    pending: HashMap<u64, Pending>,
    /// The number of each request of a live connection not yet over. A gone connection's are
    /// found by a walk and put in order of id; a request's begin and end touch one slot and
    /// allocate nothing.
    live: HashMap<RequestRef, u64>,
    /// Every job not finished, by its slot ([`JobId`]): what it is for.
    jobs: Slab<Job>,
    /// Commits waiting to be durable.
    tickets: HashMap<Ticket, u64>,
    /// The views each connection opened with [`Request::Open`], by root, one per open; sorted
    /// by root where a gone connection's are closed.
    held: HashMap<Conn, HashMap<ContentId, Vec<ViewId>>>,
    /// The database's view of the last job that ended on each of the latest [`KEPT`] roots,
    /// oldest first: what [`Request::Why`] reads, since a view's reruns are its own.
    kept: VecDeque<(ContentId, ViewId)>,
    intents: Intents,
    /// The clock as [`Session::tick`] last gave it.
    now_ns: u64,
    /// Each subsystem's allocation rate since the last [`Request::Memory`].
    rate: Rate,
    buffers: Buffers,
}

/// Buffers kept between calls, cleared and refilled, so a warm call allocates none of them.
#[derive(Default)]
struct Buffers {
    /// A best-of's started candidates.
    starts: Vec<(JobId, Opened)>,
    /// A gone connection's requests, and their sort's scratch.
    going: Vec<(RequestRef, u64)>,
    going_scratch: Vec<(RequestRef, u64)>,
    /// A gone connection's held views, by root.
    closing: Vec<(ContentId, Vec<ViewId>)>,
    /// [`Request::Status`]'s sort's scratch.
    ids_scratch: Vec<u64>,
}

/// How many roots' last builds [`Session`] keeps for [`Request::Why`].
pub const KEPT: usize = 64;

/// The most jobs under way at once: the room [`Session`]'s table of jobs reserves.
const JOBS: u32 = 1 << 20;

/// What a job is the session's for.
enum Job {
    /// A request's, by its number, and its candidate for a best-of-N.
    Request { serial: u64, candidate: Option<u32> },
    /// Declared intent's ([`intent`]).
    Intent,
}

/// The table's handle of `job`; `None` for an even generation, which no job has (a free or
/// retired slot's).
fn handle(job: JobId) -> Option<Handle<Job>> {
    (job.generation & 1 == 1)
        .then(|| Handle::from_bits((u64::from(job.generation) << 32) | u64::from(job.slot)))
}

struct Pending {
    sink: Sink,
    what: Kind,
}

/// Where a request's replies go, and whether they go at all.
#[derive(Clone, Copy)]
struct Sink {
    to: RequestRef,
    /// The connection is gone: nothing more is sent.
    gone: bool,
}

impl Sink {
    fn send(self, out: &mut Vec<Reply>, response: Response) {
        if !self.gone {
            out.push(Reply { to: self.to, response });
        }
    }
}

enum Kind {
    Commit { root: ContentId, rebased: bool, ticket: Ticket, head: String },
    /// One job of the build, with the views it holds, and the one whose database view is kept
    /// for [`Request::Why`] when it ends. `owes_work` until a build, test or run has reported
    /// [`Response::Work`]: one that ends without it fails, naming it.
    Job {
        job: JobId,
        cancelled: bool,
        /// At most two: a reach's from and to, else the one it builds (none for a snapshot).
        views: [Option<Opened>; 2],
        kept: Option<Opened>,
        owes_work: bool,
    },
    /// A [`Request::Edit`]'s job, on its base, and the ops it reported; it becomes the
    /// `Commit` of them when it ends.
    Edit {
        job: JobId,
        cancelled: bool,
        opened: Opened,
        head: String,
        ops: Option<Vec<blaze_vfs::Op>>,
    },
    BestOf(BestOf),
}

impl Kind {
    // dyn: the daemon's one builder, chosen at startup; one call per job.
    fn cancel(&mut self, builder: &mut dyn Builder) {
        match self {
            Kind::Commit { .. } => {}
            Kind::Job { job, cancelled, .. } | Kind::Edit { job, cancelled, .. } => {
                if !*cancelled {
                    *cancelled = true;
                    builder.cancel(*job);
                }
            }
            Kind::BestOf(best) => best.cancel(builder),
        }
    }

    fn views(&self) -> usize {
        match self {
            Kind::Commit { .. } => 0,
            Kind::Job { views, .. } => views.iter().flatten().count(),
            Kind::Edit { .. } => 1,
            Kind::BestOf(best) => best.views(),
        }
    }
}

impl Default for Session {
    fn default() -> Session {
        Session::new()
    }
}

impl Session {
    pub fn new() -> Session {
        Session {
            next_serial: 0,
            pending: HashMap::default(),
            live: HashMap::default(),
            // alloc: none, address space reserved.
            jobs: match Slab::new(JOBS, Tag::Api) {
                Ok(it) => it,
                Err(error) => panic!("the session's table of jobs: {error}"),
            },
            tickets: HashMap::default(),
            held: HashMap::default(),
            kept: VecDeque::new(),
            intents: Intents::default(),
            now_ns: 0,
            rate: Rate::new(),
            buffers: Buffers::default(),
        }
    }

    /// The daemon's monotonic clock is `now_ns`: the intents whose wait it passed are
    /// cancelled.
    pub fn tick(&mut self, cx: &mut Daemon<'_>, now_ns: u64) {
        self.now_ns = now_ns;
        self.intents.tick(cx.builder, now_ns);
    }

    /// `snapshot` as [`Response::Memory`] gives it, each tag's rate since the last.
    fn memory(&mut self, snapshot: Snapshot) -> Memory {
        let rates = self.rate.sample(self.now_ns, &snapshot);
        let tags = Tag::ALL
            .into_iter()
            .map(|tag| {
                let it = snapshot.get(tag);
                TagMemory {
                    // alloc: the wire value's own name, once per tag per Memory request.
                    tag: tag.name().to_owned(),
                    live: it.live,
                    rate: rates[tag.index()],
                    budget: it.budget,
                    allocated: it.allocated,
                    task_peak: it.task_peak,
                    high_water: it.high_water,
                }
            })
            // alloc: the response's list of tags, once per Memory request.
            .collect();
        Memory {
            tags,
            committed: snapshot.committed,
            committed_high_water: snapshot.committed_high_water,
        }
    }

    /// When [`Session::tick`] next has something to do, on the clock it is given.
    pub fn deadline(&self) -> Option<u64> {
        self.intents.deadline()
    }

    /// The daemon moved `head` to `root` itself (the watcher's commit): intent on the head is
    /// taken over or cancelled as by a commit request.
    pub fn head_moved(&mut self, cx: &mut Daemon<'_>, head: &str, root: ContentId) {
        self.intents.moved(cx.builder, head, root);
    }

    pub fn arrive(&mut self, cx: &mut Daemon<'_>, arrived: Arrived, out: &mut Vec<Reply>) {
        match arrived {
            Arrived::Request { from, request } => {
                if self.live.contains_key(&from) {
                    let error = ApiError::new(
                        // alloc: error path, a request id already under way.
                        format!("request {} is already under way on this connection", from.id),
                        Some(format!("request {}", from.id)), // alloc: as above.
                    );
                    fail(out, from, error);
                    return;
                }
                if let Err(error) = self.request(cx, from, request, out) {
                    fail(out, from, error);
                }
            }
            Arrived::Gone { conn } => self.gone(cx, conn),
        }
    }

    /// A job's response ([`Builder`]'s side). One of a job that is unknown or has finished is
    /// dropped: a job can finish before its cancel reaches the build.
    pub fn report(
        &mut self,
        _cx: &mut Daemon<'_>,
        job: JobId,
        response: Response,
        out: &mut Vec<Reply>,
    ) {
        assert!(
            !matches!(response, Response::Done { .. } | Response::Error(_)),
            "job {job:?} reported {response:?}: a job ends through `Session::finish`"
        );
        let Some(&Job::Request { serial, candidate }) = handle(job).and_then(|it| self.jobs.get(it))
        else {
            return;
        };
        let pending = self.pending.get_mut(&serial).expect("a job's request is pending");
        let sink = pending.sink;
        match (&mut pending.what, candidate) {
            (Kind::BestOf(best), Some(candidate)) => {
                best.count(candidate, &response);
                sink.send(out, of(candidate, response));
            }
            (Kind::Job { owes_work, .. }, _) => {
                if matches!(response, Response::Work(_)) {
                    *owes_work = false;
                }
                sink.send(out, response);
            }
            (Kind::Edit { ops, .. }, _) => match response {
                Response::Edited(edited) if sink.gone => *ops = Some(edited.ops),
                response => {
                    if let Response::Edited(edited) = &response {
                        // The ops go both to the client, in the reply, and into the commit.
                        // clone: the reply's ops and the commit's; moved when the client is gone.
                        *ops = Some(edited.ops.clone());
                    }
                    sink.send(out, response);
                }
            },
            _ => sink.send(out, response),
        }
    }

    /// A job's end: `ok`, and the error when it could not run. Its views are closed. One of a
    /// job that is unknown or has finished is dropped.
    pub fn finish(
        &mut self,
        cx: &mut Daemon<'_>,
        job: JobId,
        ok: bool,
        error: Option<ApiError>,
        out: &mut Vec<Reply>,
    ) {
        self.finish_in(cx, job, ok, error, None, out);
    }

    /// [`Session::finish`] of a job whose build ran in a database view of its own, `queries`,
    /// handed over: the session keeps that view for [`Request::Why`] in place of the one it
    /// opened, and closes it when it lets it go (at once when the job's view is not kept).
    pub fn finish_in(
        &mut self,
        cx: &mut Daemon<'_>,
        job: JobId,
        ok: bool,
        error: Option<ApiError>,
        mut queries: Option<ViewId>,
        out: &mut Vec<Reply>,
    ) {
        let Some(Job::Request { serial, candidate }) = handle(job).and_then(|it| self.jobs.remove(it))
        else {
            if let Some(opened) = self.intents.finish(job) {
                close(cx, opened);
            }
            if let Some(queries) = queries {
                cx.db.close(queries);
            }
            return;
        };
        if matches!(self.pending[&serial].what, Kind::Edit { .. }) {
            if let Some(queries) = queries {
                cx.db.close(queries);
            }
            if self.edit_ended(cx, serial, ok, error, out) {
                self.end(serial);
            }
            return;
        }
        let pending = self.pending.get_mut(&serial).expect("a job's request is pending");
        let sink = pending.sink;
        let ok = ok && error.is_none();
        let over = match (&mut pending.what, candidate) {
            (Kind::BestOf(best), Some(candidate)) => {
                let ran = error.is_none();
                match best.finish(candidate, ok, ran) {
                    Some(opened) if ran => {
                        cx.vfs.close(opened.files);
                        let opened = built_in(cx.db, opened, queries.take());
                        keep(&mut self.kept, cx.db, opened);
                    }
                    Some(opened) => close(cx, opened),
                    None => {}
                }
                if let Some(error) = error {
                    sink.send(out, of(candidate, Response::Error(error)));
                }
                sink.send(out, of(candidate, Response::Done { ok }));
                if best.running() == 0 {
                    sink.send(out, Response::Ranked(best.ranked()));
                    sink.send(out, Response::Done { ok: best.ok() });
                    true
                } else {
                    false
                }
            }
            (Kind::Job { views, kept, owes_work, cancelled, .. }, None) => {
                let error = match error {
                    None if *owes_work && !*cancelled => Some(ApiError::new(
                        // alloc: error path, a build that did not report its work.
                        format!(
                            "the build of {:x} ended without reporting what it did \
                             (`Response::Work`)",
                            views[0].expect("a build, test or run has its view").view.root
                        ),
                        Some(format!("job {job}")), // alloc: as above.
                    )),
                    error => error,
                };
                let ok = ok && error.is_none();
                for opened in views.iter_mut().filter_map(Option::take) {
                    if error.is_none() && *kept == Some(opened) {
                        cx.vfs.close(opened.files);
                        let opened = built_in(cx.db, opened, queries.take());
                        keep(&mut self.kept, cx.db, opened);
                    } else {
                        close(cx, opened);
                    }
                }
                if let Some(error) = error {
                    sink.send(out, Response::Error(error));
                }
                sink.send(out, Response::Done { ok });
                true
            }
            _ => unreachable!("job {job:?}'s candidate does not match its request"),
        };
        if let Some(queries) = queries {
            cx.db.close(queries);
        }
        if over {
            self.end(serial);
        }
    }

    /// An edit's job ended: its ops committed to its head, the request then waiting on the
    /// commit's ticket; true when the request is over instead (it failed, was cancelled, or
    /// the commit was refused).
    fn edit_ended(
        &mut self,
        cx: &mut Daemon<'_>,
        serial: u64,
        ok: bool,
        error: Option<ApiError>,
        out: &mut Vec<Reply>,
    ) -> bool {
        let pending = self.pending.get_mut(&serial).expect("a job's request is pending");
        let sink = pending.sink;
        let Kind::Edit { job, cancelled, opened, head, ops } = &mut pending.what else {
            unreachable!("job of serial {serial} is no edit")
        };
        let (job, opened, base) = (*job, *opened, opened.view.root);
        let made = match (error, ops.take()) {
            (Some(error), _) => Err(error),
            (None, _) if !ok || *cancelled => Ok(None),
            (None, None) => Err(ApiError::new(
                // alloc: error path, edits that did not report their ops.
                format!(
                    "the edits of {base:x} ended without reporting what they changed \
                     (`Response::Edited`)"
                ),
                Some(format!("job {job}")), // alloc: as above.
            )),
            (None, Some(ops)) => Ok(Some(ops)),
        };
        let head = std::mem::take(head);
        let over = match made {
            Ok(Some(ops)) => match cx.vfs.commit_owned(&head, base, ops) {
                Ok(commit) => {
                    self.intents.moved(cx.builder, &head, commit.root);
                    self.tickets.insert(commit.ticket, serial);
                    pending.what = Kind::Commit {
                        root: commit.root,
                        rebased: commit.rebased,
                        ticket: commit.ticket,
                        head,
                    };
                    false
                }
                Err(error) => {
                    sink.send(out, Response::Error(commit_error(&head, base, &error)));
                    sink.send(out, Response::Done { ok: false });
                    true
                }
            },
            Ok(None) => {
                sink.send(out, Response::Done { ok: false });
                true
            }
            Err(error) => {
                sink.send(out, Response::Error(error));
                sink.send(out, Response::Done { ok: false });
                true
            }
        };
        close(cx, opened);
        over
    }

    /// Commits the VFS now says are durable (what [`Vfs::complete`] returned). Tickets of
    /// commits the session did not make (the watcher's, a gone connection's) are not its own
    /// and are passed over.
    pub fn durable(&mut self, _cx: &mut Daemon<'_>, tickets: &[Ticket], out: &mut Vec<Reply>) {
        for ticket in tickets {
            let Some(serial) = self.tickets.remove(ticket) else { continue };
            let pending = self.end(serial);
            let Kind::Commit { root, rebased, .. } = pending.what else {
                unreachable!("ticket {ticket:?} is of a request that is no commit")
            };
            pending.sink.send(out, Response::Committed { root, rebased });
            pending.sink.send(out, Response::Done { ok: true });
        }
    }

    fn request(
        &mut self,
        cx: &mut Daemon<'_>,
        req: RequestRef,
        request: Request,
        out: &mut Vec<Reply>,
    ) -> Result<(), ApiError> {
        match request {
            Request::Commit { head, base, ops } => {
                let commit = cx
                    .vfs
                    .commit_owned(&head, base, ops)
                    .map_err(|error| commit_error(&head, base, &error))?;
                self.intents.moved(cx.builder, &head, commit.root);
                let serial = self.begin(
                    req,
                    Kind::Commit {
                        root: commit.root,
                        rebased: commit.rebased,
                        ticket: commit.ticket,
                        head,
                    },
                );
                self.tickets.insert(commit.ticket, serial);
            }
            Request::Check { view, targets } => self.build(cx, req, &view, Work::Check(targets))?,
            Request::Build { view, targets } => self.build(cx, req, &view, Work::Build(targets))?,
            Request::Test { view, targets, filter, all, args, ub } => {
                self.build(cx, req, &view, Work::Test { targets, filter, all, args, ub })?
            }
            Request::Run { view, targets, bin, args } => {
                self.build(cx, req, &view, Work::Run { targets, bin, args })?
            }
            Request::Verify { view, targets } => {
                self.build(cx, req, &view, Work::Verify(targets))?
            }
            Request::CodegenInputs { view, targets } => {
                self.build(cx, req, &view, Work::CodegenInputs(targets))?
            }
            Request::Trace { view, targets } => self.build(cx, req, &view, Work::Trace(targets))?,
            Request::Tables { view } => self.build(cx, req, &view, Work::Tables)?,
            Request::Pack { view, targets, program } => {
                self.build(cx, req, &view, Work::Pack { targets, program })?
            }
            Request::Doc { view, targets, no_deps, private } => {
                self.build(cx, req, &view, Work::Doc { targets, no_deps, private })?
            }
            Request::Reach { from, to, targets } => {
                let from = resolve(cx.vfs, &from)?;
                let to = resolve(cx.vfs, &to)?;
                let from = open(cx, from)?;
                let to = match open(cx, to) {
                    Ok(to) => to,
                    Err(error) => {
                        close(cx, from);
                        return Err(error);
                    }
                };
                let changes = cx.vfs.diff(from.view.root, to.view.root);
                let work = Work::Reach { from, changes, targets };
                self.start(cx, req, Some(to), [Some(from), Some(to)], work);
            }
            Request::Snapshot => self.start(cx, req, None, [None, None], Work::Snapshot),
            Request::Stop => out.push(Reply { to: req, response: Response::Done { ok: true } }),
            Request::Edit { base, head, edits } => {
                if !cx.vfs.has(base) {
                    return Err(commit_error(&head, base, &CommitError::UnknownBase(base)));
                }
                let opened = open(cx, base)?;
                let serial = self.serial();
                let job = self.job(Job::Request { serial, candidate: None });
                let edit = Kind::Edit { job, cancelled: false, opened, head, ops: None };
                self.begin_as(serial, req, edit);
                cx.builder.start(job, Some(opened), Work::Edit(edits));
            }
            Request::WillTest { view, filter, targets, wait_ms } => {
                let root = resolve(cx.vfs, &view)?;
                let work = Work::Ahead { targets, filter };
                self.intend(cx, req, root, None, wait_ms, work, out)?;
            }
            Request::WillSave { ops, head, test, targets, wait_ms } => {
                let base = cx.vfs.head(&head).unwrap_or_else(|| cx.vfs.empty());
                let root = cx.vfs.apply(base, &ops).map_err(|error| {
                    ApiError::new(
                        // alloc: error path, a save whose ops don't apply.
                        format!("the save's ops don't apply to head `{head}` at {base:x}: {error}"),
                        Some(op_path(&error)),
                    )
                })?;
                let work = if test {
                    // alloc: none, empty lists.
                    Work::Test { targets, filter: Vec::new(), all: false, args: Vec::new(), ub: None }
                } else {
                    Work::Check(targets)
                };
                self.intend(cx, req, root, Some(head), wait_ms, work, out)?;
            }
            Request::BestOf { base, candidates, targets, test, filter } => {
                let base = resolve(cx.vfs, &base)?;
                if candidates.is_empty() {
                    return Err(ApiError::new(
                        // alloc: error path, a best-of of no candidates.
                        format!("a best-of on {base:x} has no candidates"),
                        Some(format!("root {base:x}")), // alloc: as above.
                    ));
                }
                let sink = Sink { to: req, gone: false };
                let serial = self.serial();
                let mut best = BestOf::new(base);
                let mut starts = std::mem::take(&mut self.buffers.starts);
                starts.clear();
                for ops in &candidates {
                    let root = cx.vfs.apply(base, ops).map_err(|error| {
                        ApiError::new(
                            // alloc: error path, a candidate whose ops don't apply.
                            format!("the candidate's ops don't apply to {base:x}: {error}"),
                            Some(op_path(&error)),
                        )
                    });
                    match root.and_then(|root| open(cx, root)) {
                        Ok(opened) => {
                            self.intents.take_over(cx.builder, opened.view.root);
                            let job = self.job(Job::Request { serial, candidate: Some(best.next()) });
                            best.started(job, opened);
                            starts.push((job, opened));
                        }
                        Err(error) => {
                            let candidate = best.refused();
                            sink.send(out, of(candidate, Response::Error(error)));
                            sink.send(out, of(candidate, Response::Done { ok: false }));
                        }
                    }
                }
                if starts.is_empty() {
                    self.buffers.starts = starts;
                    sink.send(out, Response::Ranked(best.ranked()));
                    sink.send(out, Response::Done { ok: false });
                    return Ok(());
                }
                let (&(job, opened), rest) = starts.split_last().expect("a candidate started");
                let work = |targets: Targets, filter: Vec<String>| {
                    if test {
                        // alloc: none, an empty list.
                        Work::Test { targets, filter, all: false, args: Vec::new(), ub: None }
                    } else {
                        Work::Check(targets)
                    }
                };
                self.begin_as(serial, req, Kind::BestOf(best));
                for &(job, opened) in rest {
                    // Each candidate's job owns its work; the last is given the request's own.
                    // clone: one per candidate but the last; a check copies no filter.
                    let filter = if test { filter.clone() } else { Vec::new() };
                    // clone: as above.
                    cx.builder.start(job, Some(opened), work(targets.clone(), filter));
                }
                cx.builder.start(job, Some(opened), work(targets, filter));
                self.buffers.starts = starts;
            }
            Request::Diff { from, to } => {
                let from = resolve(cx.vfs, &from)?;
                let to = resolve(cx.vfs, &to)?;
                let changes = cx.vfs.diff(from, to);
                answer(out, req, Response::Diff { changes });
            }
            Request::Why { view, query } => {
                let root = resolve(cx.vfs, &view)?;
                let Some(&(_, queries)) = self.kept.iter().find(|(kept, _)| *kept == root) else {
                    return Err(ApiError::new(
                        // alloc: error path, a root with no kept build.
                        format!(
                            "no build of {root:x} has ended among the latest {KEPT} roots built: \
                             build it, then ask why"
                        ),
                        Some(format!("root {root:x}")), // alloc: as above.
                    ));
                };
                let Some(why) = cx.db.why(queries, &query) else {
                    return Err(ApiError::new(
                        // alloc: error path, a query that did not rerun.
                        format!("{} did not rerun in the last build of {root:x}", query_name(&query)),
                        Some(query_name(&query)),
                    ));
                };
                answer(out, req, Response::Why(why));
            }
            Request::Status => {
                // alloc: the reply's own list of requests, once per Status; none when idle.
                let mut requests: Vec<u64> = Vec::with_capacity(self.live.len());
                requests.extend(self.live.keys().map(|it| it.id));
                radix::sort_copy_by_key_in(&mut requests, |it| *it, &mut self.buffers.ids_scratch);
                let status = Status {
                    pid: cx.pid,
                    // clone: the reply's own copy, once per Status.
                    build: cx.build.clone(),
                    requests,
                    views: self.views(),
                    // alloc: the reply's own copy of an error, once per Status.
                    history: cx.history.map(str::to_owned),
                    memory_bytes: cx.memory_bytes,
                    budget_bytes: cx.budget_bytes,
                };
                answer(out, req, Response::Status(status));
            }
            Request::Memory => {
                let memory = self.memory((cx.memory)());
                answer(out, req, Response::Memory(memory));
            }
            Request::Profile { seconds } => {
                let trace = (cx.profile)(std::time::Duration::from_secs(u64::from(seconds)));
                answer(out, req, Response::Profiled { trace });
            }
            Request::Hot { top, since } => {
                let hot = (cx.hot)(top, since)?;
                answer(out, req, Response::Hot(hot));
            }
            Request::Explore { .. } | Request::Live | Request::Sql { .. } => {
                return Err(ApiError::new(
                    "the explorer, the live feed and live SQL are answered by the daemon's loop, \
                     which holds their servers and feeds; this one handed the request on instead",
                    Some(request_name(&request).to_owned()),
                ));
            }
            Request::Open { view } => {
                let root = resolve(cx.vfs, &view)?;
                let files = cx.vfs.open(View { root }).map_err(|error| open_error(root, &error))?;
                self.held.entry(req.conn).or_default().entry(root).or_default().push(files);
                answer(out, req, Response::Opened { root });
            }
            Request::Close { root } => {
                let Some(held) = self.held.get_mut(&req.conn).filter(|it| it.contains_key(&root))
                else {
                    return Err(ApiError::new(
                        // alloc: error path, a close of a view not held.
                        format!("this connection holds no open view of {root:x}"),
                        Some(format!("root {root:x}")), // alloc: as above.
                    ));
                };
                let views = held.get_mut(&root).expect("a held root");
                let files = views.pop().expect("a held root has an open view");
                if views.is_empty() {
                    held.remove(&root);
                }
                if held.is_empty() {
                    self.held.remove(&req.conn);
                }
                cx.vfs.close(files);
                out.push(Reply { to: req, response: Response::Done { ok: true } });
            }
            Request::Cancel { id } => {
                let target = RequestRef { conn: req.conn, id };
                let Some(&serial) = self.live.get(&target) else {
                    return Err(ApiError::new(
                        // alloc: error path, a cancel of no request under way.
                        format!("no request {id} is under way on this connection"),
                        Some(format!("request {id}")), // alloc: as above.
                    ));
                };
                let pending = self.pending.get_mut(&serial).expect("a live request is pending");
                if let Kind::Commit { head, .. } = &pending.what {
                    return Err(ApiError::new(
                        // alloc: error path, a cancel of a commit already made.
                        format!(
                            "request {id} is a commit to head `{head}` that is made and waiting \
                             to be durable: it can't be cancelled"
                        ),
                        Some(format!("request {id}")), // alloc: as above.
                    ));
                }
                pending.what.cancel(cx.builder);
                out.push(Reply { to: req, response: Response::Done { ok: true } });
            }
        }
        Ok(())
    }

    /// A request on one view, built by one job.
    fn build(
        &mut self,
        cx: &mut Daemon<'_>,
        req: RequestRef,
        view: &ViewRef,
        work: Work,
    ) -> Result<(), ApiError> {
        let root = resolve(cx.vfs, view)?;
        let opened = open(cx, root)?;
        self.start(cx, req, Some(opened), [Some(opened), None], work);
        Ok(())
    }

    fn start(
        &mut self,
        cx: &mut Daemon<'_>,
        req: RequestRef,
        view: Option<Opened>,
        views: [Option<Opened>; 2],
        work: Work,
    ) {
        let serial = self.serial();
        let job = self.job(Job::Request { serial, candidate: None });
        let kept = view.filter(|_| incremental(&work));
        if let Some(kept) = kept {
            self.intents.take_over(cx.builder, kept.view.root);
        }
        let owes_work = matches!(work, Work::Build(_) | Work::Test { .. } | Work::Run { .. });
        self.begin_as(serial, req, Kind::Job { job, cancelled: false, views, kept, owes_work });
        cx.builder.start(job, view, work);
    }

    /// Declared intent: `work` on `root` started through [`Builder::speculate`], belonging to
    /// no request, and the request answered at once.
    fn intend(
        &mut self,
        cx: &mut Daemon<'_>,
        req: RequestRef,
        root: ContentId,
        head: Option<String>,
        wait_ms: u64,
        work: Work,
        out: &mut Vec<Reply>,
    ) -> Result<(), ApiError> {
        let opened = open(cx, root)?;
        let job = self.job(Job::Intent);
        self.intents.add(cx.builder, req.conn, job, opened, head, wait_ms);
        cx.builder.speculate(job, opened, work);
        answer(out, req, Response::Announced { root });
        Ok(())
    }

    /// A slot of the table for `job`, a freed one before a new one ([`JobId`]): an index, no
    /// hash, and no allocation once the table has grown to the jobs under way at once.
    fn job(&mut self, job: Job) -> JobId {
        let handle = self.jobs.insert(job);
        JobId { slot: handle.index(), generation: handle.generation() }
    }

    /// The number of a request about to begin ([`Session::begin_as`]), for its jobs to name.
    fn serial(&mut self) -> u64 {
        let serial = self.next_serial;
        self.next_serial += 1;
        serial
    }

    fn begin(&mut self, req: RequestRef, what: Kind) -> u64 {
        let serial = self.serial();
        self.begin_as(serial, req, what);
        serial
    }

    fn begin_as(&mut self, serial: u64, req: RequestRef, what: Kind) {
        self.pending.insert(serial, Pending { sink: Sink { to: req, gone: false }, what });
        self.live.insert(req, serial);
    }

    fn end(&mut self, serial: u64) -> Pending {
        let pending = self.pending.remove(&serial).expect("an ended request was pending");
        if !pending.sink.gone {
            self.live.remove(&pending.sink.to);
        }
        pending
    }

    fn gone(&mut self, cx: &mut Daemon<'_>, conn: Conn) {
        let mut requests = std::mem::take(&mut self.buffers.going);
        requests.clear();
        requests.extend(
            self.live.iter().filter(|(req, _)| req.conn == conn).map(|(req, serial)| (*req, *serial)),
        );
        radix::sort_copy_by_key_in(&mut requests, |it| it.0.id, &mut self.buffers.going_scratch);
        for &(req, serial) in &requests {
            self.live.remove(&req);
            let pending = self.pending.get_mut(&serial).expect("a live request is pending");
            pending.sink.gone = true;
            if let Kind::Commit { ticket, .. } = pending.what {
                self.pending.remove(&serial);
                self.tickets.remove(&ticket);
            } else {
                pending.what.cancel(cx.builder);
            }
        }
        self.buffers.going = requests;
        self.intents.gone(cx.builder, conn);
        let Some(held) = self.held.remove(&conn) else { return };
        let closing = &mut self.buffers.closing;
        closing.clear();
        closing.extend(held);
        closing.sort_unstable_by_key(|&(root, _)| root);
        for files in closing.drain(..).flat_map(|(_, views)| views) {
            cx.vfs.close(files);
        }
    }

    /// The views the session holds open: the connections' and the jobs'.
    fn views(&self) -> u32 {
        let held: usize = self.held.values().flat_map(HashMap::values).map(Vec::len).sum();
        let jobs: usize = self.pending.values().map(|it| it.what.views()).sum();
        u32::try_from(held + jobs + self.intents.views()).expect("fewer than 2^32 open views")
    }
}

fn answer(out: &mut Vec<Reply>, to: RequestRef, response: Response) {
    out.push(Reply { to, response });
    out.push(Reply { to, response: Response::Done { ok: true } });
}

fn of(candidate: u32, response: Response) -> Response {
    // alloc: the wire type's box of a candidate's response.
    Response::Of { candidate, response: Box::new(response) }
}

fn fail(out: &mut Vec<Reply>, to: RequestRef, error: ApiError) {
    out.push(Reply { to, response: Response::Error(error) });
    out.push(Reply { to, response: Response::Done { ok: false } });
}

/// The root a request names: a head never written is the empty tree.
fn resolve(vfs: &mut Vfs, view: &ViewRef) -> Result<ContentId, ApiError> {
    match view {
        ViewRef::Head(head) => Ok(vfs.head(head).unwrap_or_else(|| vfs.empty())),
        ViewRef::Root(view) => {
            if vfs.has(view.root) {
                Ok(view.root)
            } else {
                Err(ApiError::new(
                    // alloc: error path, an unknown root.
                    format!("no root {:x} is known", view.root),
                    Some(format!("root {:x}", view.root)), // alloc: as above.
                ))
            }
        }
        ViewRef::WhatIf { base, ops } => {
            let base = resolve(vfs, base)?;
            vfs.apply(base, ops).map_err(|error| {
                ApiError::new(
                    // alloc: error path, a what-if whose ops don't apply.
                    format!("the what-if's ops don't apply to {base:x}: {error}"),
                    Some(op_path(&error)),
                )
            })
        }
    }
}

/// `root` opened in both the VFS and the database, for a job.
fn open(cx: &mut Daemon<'_>, root: ContentId) -> Result<Opened, ApiError> {
    let view = View { root };
    let files = cx.vfs.open(view).map_err(|error| open_error(root, &error))?;
    let queries = cx.db.open(view);
    Ok(Opened { view, files, queries })
}

fn close(cx: &mut Daemon<'_>, opened: Opened) {
    cx.vfs.close(opened.files);
    cx.db.close(opened.queries);
}

/// Whether `work` runs the view's queries incrementally, so their reruns answer
/// [`Request::Why`]: a reach builds nothing, a verify builds from nothing, tables only count.
fn incremental(work: &Work) -> bool {
    match work {
        Work::Check(_)
        | Work::Build(_)
        | Work::Test { .. }
        | Work::Run { .. }
        | Work::CodegenInputs(_)
        | Work::Trace(_)
        | Work::Pack { .. }
        | Work::Doc { .. } => true,
        Work::Reach { .. }
        | Work::Verify(_)
        | Work::Snapshot
        | Work::Tables
        | Work::Edit(_)
        | Work::Ahead { .. } => false,
    }
}

/// `opened` with the database view its build ran in, when the daemon handed one over: the
/// session's own is closed for it.
fn built_in(db: &blaze_query::Database, opened: Opened, queries: Option<ViewId>) -> Opened {
    match queries {
        Some(queries) => {
            db.close(opened.queries);
            Opened { queries, ..opened }
        }
        None => opened,
    }
}

/// `opened`'s database view kept as its root's last build, closing the one it replaces and,
/// past [`KEPT`] roots, the oldest.
fn keep(kept: &mut VecDeque<(ContentId, ViewId)>, db: &blaze_query::Database, opened: Opened) {
    let root = opened.view.root;
    if let Some(at) = kept.iter().position(|(it, _)| *it == root) {
        let (_, old) = kept.remove(at).expect("a kept root");
        db.close(old);
    }
    if kept.len() == KEPT {
        let (_, oldest) = kept.pop_front().expect("KEPT roots are kept");
        db.close(oldest);
    }
    kept.push_back((root, opened.queries));
}

fn open_error(root: ContentId, error: &CommitError) -> ApiError {
    // alloc: error path, a view that could not be opened.
    ApiError::new(format!("opening {root:x}: {error}"), Some(format!("root {root:x}")))
}

fn commit_error(head: &str, base: ContentId, error: &CommitError) -> ApiError {
    match error {
        CommitError::UnknownBase(root) => ApiError::new(
            // alloc: error path, a commit on an unknown base.
            format!("the base {root:x} of a commit to head `{head}` is no root this daemon has"),
            Some(head.to_owned()), // alloc: as above.
        ),
        CommitError::Conflict { path } => ApiError::new(
            // alloc: error path, a commit that conflicts.
            format!(
                "head `{head}` moved past {base:x} and changed {path} in a way the commit's ops \
                 can't be moved past"
            ),
            Some(path.clone()), // clone: as above, the error's own path.
        ),
        CommitError::Op(error) => ApiError::new(
            // alloc: error path, a commit whose op failed.
            format!("a commit to head `{head}` on {base:x}: {error}"),
            Some(op_path(error)),
        ),
    }
}

/// Where an op failed: its path, or the root it was given.
fn op_path(error: &OpError) -> String {
    match error {
        OpError::BadPath(path)
        | OpError::NotFound(path)
        | OpError::IsADirectory(path)
        | OpError::NotADirectory(path)
        | OpError::Exists(path)
        | OpError::OutOfRange { path, .. }
        // clone: error path, the error's own path.
        | OpError::BadLink { path, .. } => path.clone(),
        // alloc: error path, the root an op was given.
        OpError::UnknownRoot(root) => format!("root {root:x}"),
        // alloc: error path, the image's group whose bytes failed their check.
        OpError::Corrupt { section, group } => format!("image section {section} group {group}"),
    }
}

/// The command of a request the session does not answer itself.
fn request_name(request: &Request) -> &'static str {
    match request {
        Request::Explore { .. } => "blaze explore",
        Request::Live => "blaze debug live",
        Request::Sql { .. } => "blaze debug sql --live",
        _ => "a request",
    }
}

fn query_name(query: &QueryKey) -> String {
    // alloc: error path, a query's name for a Why that found nothing.
    format!("the query of kind {:#06x} on {:x}", query.kind.0, query.input)
}
