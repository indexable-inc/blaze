//! What each build is doing, looked at about once a second while one runs ([`Jobs::doing`]):
//! said to its client as [`Response::Doing`] (what it waits on while none of its units
//! compiles, what each unit under way is inside, what the busy workers run), and kept for
//! `blaze status` ([`Building`]).
//!
//! What a build waits on is, first, a download in flight (a byte arriving on it is the build
//! moving); then the longest open span of a thread working for its plane request (the
//! recorder's build of each span, [`OpenSpan::build`]); then its stage ([`StageSaid`]). A
//! unit is matched to the thread working for it by the plane node of that thread's `task`
//! span ([`OpenSpan::node`]), never by its name. Downloads are the process's
//! (`blaze_fetch::downloads`), which no request owns: a build that waits on nothing else
//! while one is in flight is said to wait on it, as a cold build waits on its pinned LLVM.

use std::{fmt::Write as _, time::Duration};

use blaze_api::{Building, Doing, Downloading, Response, UnitDoing};
use blaze_fetch::{
    downloads::{Downloads, Shown},
    progress::{Bytes, Secs},
};
use blaze_graph::{RequestId, machine::NodeId};
use blaze_profile::flight::{OpenSpan, ThreadNow};

use super::{Job, Jobs, Said, Stage, StageSaid, Steps, command_line, targets_of};

/// How often a build's [`Response::Doing`] is said, and how long a build runs before its
/// first: a warm build that ends within it says none.
pub const EVERY: Duration = Duration::from_secs(1);

/// How many threads `blaze status` names for each build ([`Building::spans`]); the rest are
/// counted.
const THREADS_NAMED: usize = 4;

/// What one look reads of the daemon besides the jobs, read once for every build.
pub struct Look<'a> {
    /// The injected clock ([`super::World::now`]).
    pub now: Duration,
    /// Each plane node a thread task of runs now, with its request (the plane's unsettled
    /// nodes in [`blaze_protocol::events::NodeState::Running`], the loop's own left out).
    pub running: &'a [(RequestId, NodeId)],
    /// Every thread inside a span ([`blaze_profile::flight::threads_now`]).
    pub threads: &'a [ThreadNow],
    /// Every download in flight, oldest first ([`Downloads::shown`]).
    pub downloads: &'a [Shown],
    /// The worker pool's workers running a job's code, and the rest.
    pub busy: u32,
    pub idle: u32,
}

impl<S: Steps> Jobs<S> {
    /// One look at every build a client waits on: each due one ([`EVERY`] since its last, and
    /// running at least that long) said its [`Response::Doing`] into `said`, and every one
    /// put into `builds`, in the order the jobs started, for `blaze status`.
    pub fn doing(&mut self, look: &Look<'_>, said: &mut Vec<Said>, builds: &mut Vec<Building>) {
        for id in &self.order {
            let Some(job) = self.jobs.get_mut(super::key_of(*id)) else { continue };
            if job.idle || matches!(job.stage, Stage::Over) || targets_of(&job.work).is_none() {
                continue;
            }
            let seen = Seen::of(job, look);
            let for_ms = u64::try_from(look.now.saturating_sub(job.began).as_millis())
                .unwrap_or(u64::MAX);
            builds.push(seen.building(job, look, for_ms));
            let due = look.now.saturating_sub(job.began) >= EVERY
                && job.doing_said.is_none_or(|at| look.now.saturating_sub(at) >= EVERY);
            if !due {
                continue;
            }
            job.doing_said = Some(look.now);
            let doing = Doing {
                on: (!seen.compiling).then(|| seen.on(job, look)),
                busy: look.busy,
                idle: look.idle,
                units: seen.units,
                for_ms,
                running: seen.longest.and_then(|it| it.open.last()).map(said_span),
            };
            said.push(Said::Report(*id, Response::Doing(doing)));
        }
    }
}

/// What one look found of one job.
struct Seen<'a> {
    /// Its plane request's number, as the recorder's spans carry it; `None` before it has one.
    request: Option<u32>,
    /// Whether a node of one of its units runs now.
    compiling: bool,
    running_units: u32,
    units: Vec<UnitDoing>,
    /// Its threads, the one inside its longest open span first.
    threads: Vec<&'a ThreadNow>,
    longest: Option<&'a ThreadNow>,
}

impl<'a> Seen<'a> {
    fn of<S: Steps>(job: &Job<S>, look: &Look<'a>) -> Seen<'a> {
        let request = job.request.map(|it| it.0);
        // alloc: a look, once a second while a build runs
        let mut threads: Vec<&ThreadNow> = match request {
            Some(build) => look
                .threads
                .iter()
                .filter(|thread| thread.open.iter().any(|span| span.build == build))
                .collect(),
            None => Vec::new(),
        };
        // The thread inside the oldest span of the build first: what it has waited on longest.
        let age = |thread: &&ThreadNow| {
            let build = request.unwrap_or(0);
            thread.open.iter().find(|span| span.build == build).map_or(0, |span| span.for_ns)
        };
        threads.sort_by(|a, b| age(b).cmp(&age(a)).then_with(|| a.tid.cmp(&b.tid)));
        let (mut compiling, mut running_units) = (false, 0);
        let mut units: Vec<UnitDoing> = Vec::new(); // alloc: a look, as above
        if let (Stage::Units(build), Some(request)) = (&job.stage, job.request) {
            for (_, node) in look.running.iter().filter(|(of, _)| *of == request) {
                let Some(at) = build.unit(*node) else { continue };
                compiling = true;
                running_units += 1;
                let Ok(id) = u32::try_from(at) else { continue };
                let on_node = threads.iter().find(|thread| {
                    thread
                        .open
                        .iter()
                        .any(|span| span.build == request.0 && span.node == Some(u64::from(node.0)))
                });
                if let Some(thread) = on_node {
                    units.push(UnitDoing { id, doing: unit_doing(thread, look.downloads) });
                }
            }
            units.sort_by_key(|it| it.id);
            units.dedup_by_key(|it| it.id);
        }
        let longest = threads.first().copied();
        Seen { request, compiling, running_units, units, threads, longest }
    }

    /// What the build waits on with none of its units compiling: a download, the innermost
    /// span of the thread inside its longest open span, or its stage.
    fn on<S: Steps>(&self, job: &Job<S>, look: &Look<'_>) -> String {
        if let Some(download) = downloading(look.downloads) {
            return download;
        }
        if let Some(span) = self.longest.and_then(|it| it.open.last()) {
            return said_span(span);
        }
        StageSaid(&job.stage).to_string() // alloc: a look, once a second while a build waits
    }

    /// The job as `blaze status` lists it.
    fn building<S: Steps>(&self, job: &Job<S>, look: &Look<'_>, for_ms: u64) -> Building {
        let phase = match &job.stage {
            // alloc: a look, once a second while a build runs
            Stage::Units(build) => format!(
                "builds {} units, {} running",
                build.input.units.graph.units.len(),
                self.running_units
            ),
            stage => StageSaid(stage).to_string(), // alloc: as above
        };
        let mut spans: Vec<String> = Vec::new(); // alloc: as above
        for thread in self.threads.iter().take(THREADS_NAMED) {
            spans.push(thread_line(thread, self.request.unwrap_or(0)));
        }
        if self.threads.len() > THREADS_NAMED {
            spans.push(format!("and {} more threads", self.threads.len() - THREADS_NAMED)); // alloc: as above
        }
        if !self.compiling && spans.is_empty() {
            // Nothing of it on a thread: what it waits on is its stage, or a download.
            if let Some(download) = downloading(look.downloads) {
                spans.push(download);
            }
        }
        Building {
            id: self.request.unwrap_or(0),
            what: command_line(&job.work),
            phase,
            spans,
            for_ms,
        }
    }
}

/// `downloading llvm-….tar.xz: 34.0 of 410.0 MB (8%), 260.0 kB/s, 24 min 10 s left`, and how
/// many more are in flight; `None` with none.
fn downloading(downloads: &[Shown]) -> Option<String> {
    let first = downloads.first()?;
    let mut said = format!("downloading {}: {}", first.what, first.line); // alloc: a look, once a second
    match downloads.len() {
        1 => {}
        2 => said.push_str(", and 1 more download"),
        n => _ = write!(said, ", and {} more downloads", n - 1),
    }
    Some(said)
}

/// What a unit's thread is inside: its innermost span with its age, or, under a span of a
/// download (a pinned native input laid out, a fetch), the download and how far it has come.
fn unit_doing(thread: &ThreadNow, downloads: &[Shown]) -> String {
    if thread.open.iter().any(|span| downloads_in(span.name))
        && let Some(first) = downloads.first()
    {
        let mut said = format!("waiting on the download of {} ({}", first.what, Bytes(first.line.done)); // alloc: a look, once a second
        if let Some(total) = first.line.total {
            _ = write!(said, " of {}", Bytes(total));
        }
        said.push(')');
        return said;
    }
    thread.open.last().map_or_else(String::new, said_span)
}

/// Whether a span of this name downloads, or waits on what does: the daemon's pinned native
/// inputs (`serve::native`), a plan's fetches, and the download itself (`blaze_fetch`).
fn downloads_in(name: &str) -> bool {
    matches!(name, "pinned native input" | "lay out pinned native input" | "fetch")
        || name.contains("download")
}

/// `unit bodies `app::main` in app, awaiting <key> (2 min 05 s)`: a span as one line says it.
fn said_span(span: &OpenSpan) -> String {
    let mut said = String::from(span.name); // alloc: a look, once a second
    if let Some(function) = &span.function {
        _ = write!(said, " `{function}`");
    }
    if let Some(krate) = &span.krate {
        _ = write!(said, " in {krate}");
    }
    if let Some(awaits) = &span.awaits {
        _ = write!(said, ", awaiting {awaits}");
    }
    _ = write!(said, " ({})", Secs(Duration::from_nanos(span.for_ns)));
    said
}

/// `blaze-worker-3: task `unit bodies (infer)` (2 min 05 s) > … > infer_query `app::main` in
/// app (1 s)`: a thread's outermost span of `build` and its innermost.
fn thread_line(thread: &ThreadNow, build: u32) -> String {
    let outer = thread.open.iter().position(|span| span.build == build).unwrap_or(0);
    let mut line = format!("{}:", thread.name); // alloc: a look, once a second
    let open = &thread.open[outer..];
    if let Some(first) = open.first() {
        _ = write!(line, " {}", said_span(first));
    }
    match open.len() {
        0 | 1 => {}
        2 => _ = write!(line, " > {}", said_span(&open[1])),
        _ => _ = write!(line, " > … > {}", said_span(&open[open.len() - 1])),
    }
    if thread.deeper > 0 {
        _ = write!(line, " (and {} spans deeper)", thread.deeper);
    }
    line
}

/// Every download in flight, for `blaze status` ([`blaze_api::Status::downloads`]): the
/// session's `downloads`. Allocates nothing when there is none.
pub fn downloads() -> Vec<Downloading> {
    let shown = Downloads::process().shown();
    if shown.is_empty() {
        return Vec::new(); // alloc: none
    }
    let ms = |it: Duration| u64::try_from(it.as_millis()).unwrap_or(u64::MAX);
    shown
        .into_iter()
        .map(|it| Downloading {
            url: it.url,
            done: it.line.done,
            rate: it.line.rate.map(|rate| rate as u64),
            what: it.what,
            total: it.line.total,
            idle_ms: ms(it.line.idle),
            elapsed_ms: ms(it.line.elapsed),
        })
        .collect() // alloc: a status's list, a handful
}
