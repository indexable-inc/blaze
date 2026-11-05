//! The daemon's collector (docs/gc.md): [`blaze_gc::Collector`] on the daemon's loop, its
//! file system work on the worker pool. The loop only feeds it (the clocks, whether the daemon
//! is idle, the checkouts it watches, the leases let go) and takes its decisions; each scan,
//! rename and unlink runs as a pool job in the class the collector's [`Priority`] names (a slice
//! without pressure only when nothing else is queued, a busy slice or an asked run with builds,
//! an urgent run ahead of them), and comes back over a channel the loop drains on its next turn.
//! Nothing here holds a lock, and no build waits on it but one held for space ([`Gc::hold`]): a
//! build's only other contact with the collector is its build dir's lease
//! ([`blaze_gc::lease::Lease`]), which waits at most for one rename.
//!
//! What it collects: `<root>/build` and `<root>/.blaze-trash` of blaze's root, and
//! `<machine>/std` and `<machine>/.blaze-trash` of the machine's shared directory
//! ([`super::machine`]; one area when it is the root), each only while it holds the area's
//! collector lock ([`blaze_gc::lease::collector_lock`]): a second daemon on the same root, or
//! `blaze cache gc`, leaves an area the first one holds alone, says so, and tries again at its
//! next scan. Its ledger is `<state>/gc-ledger.json`.
//!
//! How hard it works is paced by each area's volume ([`blaze_gc::pace`]): the loop measures
//! free space (one `statvfs` an area) as a build starts, every second while builds run (every
//! half minute while idle), and right after each run ends, and hands it to the collector, whose
//! slices grow with the pressure. Free space at a volume's floor while a build runs asks for an
//! urgent run ([`Collector::urgent`], free space only: a share overrun is paced, never urgent);
//! a build that fails for want of space is held until the urgent run it asked for ends and the
//! volumes are measured after it ([`Gc::hold`], [`Gc::take_released`]).
//!
//! A job waits in the pool's queue behind other work, so what the collector checked as it asked
//! may have changed by the time it runs: each carries a [`Gate`] that it checks once it holds
//! the lease, just before it renames. A lease taken in this process since the ask (a build that
//! began, [`blaze_gc::lease::uses`]) or a head that changed stops the rename, and so does the
//! run's end or its hard deadline. The leases taken are marked used in the collector each turn
//! ([`blaze_gc::lease::take_used`]), so a later step of the run keeps them too. An answer that
//! comes back after its run ended is the collector's to drop ([`Collector::scanned`]); the loop
//! never waits for one.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use blaze_api::{Reply, RequestRef, Response};
use blaze_gc::{
    Collector, Config, Decision, Ledger, RunReport,
    fs::{Answer, Gate, Sizes},
    pace::Levels,
    plan::{NoSpace, Priority, Released, Trigger},
};
use blaze_graph::pool::Class;
use crossbeam_channel::{Receiver, Sender};

/// How often the volumes are measured while builds run, and while the daemon is idle.
const MEASURE_BUSY_NS: u64 = 1_000_000_000;
const MEASURE_IDLE_NS: u64 = 30_000_000_000;
/// How often at most the ledger is written for a paced run; an asked or urgent run's is
/// written as it ends.
const LEDGER_EVERY_NS: u64 = 60_000_000_000;

/// What a background job hands back to the loop.
enum Back {
    Answer(Answer),
    /// A scan's answer, and the sizes it measured through, handed back for the next scan.
    Scanned(Answer, Sizes),
    /// The ledger was written, or why not.
    Wrote(Result<(), String>),
}

pub struct Gc {
    collector: Collector,
    ledger: PathBuf,
    send: Sender<Back>,
    back: Receiver<Back>,
    /// The last run that ended: what is still queued of it is not done ([`Gate::ended`]).
    ended: Arc<AtomicU64>,
    /// A ledger write under way; the newest ledger waiting for it.
    writing: bool,
    unwritten: Option<Ledger>,
    /// A paced run ended whose ledger is not written yet, and when the last one was.
    ledger_due: bool,
    written_ns: u64,
    /// Why the ledger did not read as the daemon started, or the last write failed: kept for
    /// status, and the ledger begun afresh only after it is said.
    pub problem: Option<String>,
    /// The checkouts last given to the collector, sorted.
    heads: Vec<String>,
    idle: bool,
    /// When the volumes were last measured.
    measured_ns: u64,
    /// [`blaze_gc::lease::releases`] as last seen.
    releases: u64,
    /// The sizes the scans measure through: out with a scan job, back with its answer.
    sizes: Option<Sizes>,
    /// Each area's collector lock, while this daemon holds it; the areas as the jobs see
    /// them, `""` for one another collector holds.
    locks: Vec<Option<stdx::file_lock::Locked>>,
    held_areas: Vec<String>,
    /// `blaze cache gc`s asked of this daemon while it holds the areas (review N14), each
    /// with its dry run, answered by the asked run of the same kind as it records.
    askers: Vec<(RequestRef, bool)>,
    /// Their answers, for the daemon to send ([`Gc::take_answers`]).
    answers: Vec<Reply>,
}

/// The areas of a daemon whose root is `root` (`<root>/build` is its build dirs') and whose
/// machine's shared directory is `machine` (`<machine>/std`).
pub fn areas(root: &Path, machine: &Path) -> Vec<String> {
    // alloc: once per daemon, an area or two
    let mut areas = vec![root.to_string_lossy().into_owned()];
    if machine != root {
        areas.push(machine.to_string_lossy().into_owned());
    }
    areas
}

/// The pool's class for decisions of `priority`.
fn class(priority: Priority) -> Class {
    match priority {
        Priority::Background => Class::Background,
        Priority::Normal => Class::Normal,
        Priority::Critical => Class::Critical,
    }
}

impl Gc {
    /// The collector of `areas`, its ledger in `state`: the last run's end and reports
    /// restored from it, so the periodic deadline does not restart with the daemon.
    pub fn new(areas: Vec<String>, state: &Path, levels: Levels) -> Gc {
        let ledger = state.join(blaze_gc::ledger::LEDGER);
        let count = areas.len();
        let mut collector = Collector::new(Config { levels, ..Config::daemon(areas) });
        let problem = match blaze_gc::ledger::read(&ledger) {
            Ok(Some(kept)) => {
                collector.restore(&kept);
                None
            }
            Ok(None) => None,
            Err(why) => {
                tracing::warn!(target: "blaze", "the collector's ledger did not read; it begins afresh: {why}");
                Some(why)
            }
        };
        let (send, back) = crossbeam_channel::unbounded();
        // From now on each lease this process takes is marked used in the collector.
        blaze_gc::lease::collecting();
        let mut gc = Gc {
            collector,
            ledger,
            send,
            back,
            ended: Arc::new(AtomicU64::new(0)), // alloc: once per daemon, shared with its jobs
            writing: false,
            unwritten: None,
            ledger_due: false,
            written_ns: 0,
            problem,
            heads: Vec::new(), // alloc: none until a checkout is watched
            idle: false,
            measured_ns: 0,
            releases: blaze_gc::lease::releases(),
            sizes: Some(Sizes::new()),
            locks: (0..count).map(|_| None).collect(), // alloc: once per daemon, an area or two
            held_areas: vec![String::new(); count],    // alloc: once per daemon, an area or two
            askers: Vec::new(),                        // alloc: none until `blaze cache gc` asks
            answers: Vec::new(),                       // alloc: none until `blaze cache gc` asks
        };
        gc.lock_areas();
        gc
    }

    /// Each area's collector lock taken that is not held yet; an area another collector holds
    /// is said, given to the jobs as `""` until a later try takes it, and named with its holder
    /// to the collector, whose [`NoSpace`] then says it was not collected (review N15).
    fn lock_areas(&mut self) {
        let holder = format!("blaze serve pid {}", std::process::id());
        // alloc: the areas not collected, an area or two, once a scan
        let mut skipped = Vec::new();
        for (at, area) in self.collector.config().areas.iter().enumerate() {
            if self.locks[at].is_some() {
                continue;
            }
            match blaze_gc::lease::collector_lock(Path::new(area), &holder) {
                Ok(Ok(file)) => {
                    self.locks[at] = Some(file);
                    // clone: the jobs' copy of the area, once it is taken
                    self.held_areas[at] = area.clone();
                }
                Ok(Err(by)) => {
                    tracing::info!(target: "blaze", area = %area, holder = %by, "{by} holds this area's {}; it is left alone until it lets go", blaze_gc::lease::COLLECTOR);
                    skipped.push(format!("{area} (held by {by})"));
                }
                Err(why) => {
                    tracing::warn!(target: "blaze", area = %area, "taking the area's collector lock: {why}; the area is left alone");
                    skipped.push(format!("{area} ({why})"));
                    self.problem = Some(why);
                }
            }
        }
        self.collector.skipped(skipped);
    }

    /// A volume below its floor after the last urgent run, as measured after it: what holds the
    /// space, for status.
    pub fn no_space(&self) -> Option<&NoSpace> {
        self.collector.no_space()
    }

    /// A build ran out of space on the volume of the collector's area `area` (`None`: the
    /// error named no path, and a volume is below its floor): it is held, under the returned
    /// id, until an urgent run that starts now (ending a paced one under way) has freed the
    /// space and the volumes are measured after it, or the collector's hold bound passes
    /// ([`Collector::hold_for_space`]); [`Gc::take_released`] says when and how. The volumes
    /// are measured every [`MEASURE_BUSY_NS`] while a build is held, so it is let go soon after.
    pub fn hold(&mut self, area: Option<usize>) -> u64 {
        let id = self.collector.hold_for_space(area);
        // A paced run it preempted ended now, outside a turn: what of it is queued on the pool
        // is not done from here on (review round 5, L1).
        self.raise_ended();
        tracing::info!(target: "blaze", hold = id, area = ?area, "a build ran out of space: it waits for an urgent gc run");
        id
    }

    /// A build that failed for want of space at `at` (its error's place) held ([`Gc::hold`])
    /// only when the collector can free that space (review N7): `at` an absolute path on the
    /// volume of one of its areas, compared by volume id ([`blaze_gc::space::volume`]); with no
    /// path, only while a collected volume is below its floor. `None`: not held, as what is
    /// full is not blaze's to free, or its volume did not measure (said, naming it).
    pub fn hold_for(&mut self, at: Option<&str>) -> Option<u64> {
        let path = at.map(Path::new).filter(|it| it.is_absolute());
        let Some(path) = path else {
            return self.collector.below_floor().then(|| self.hold(None));
        };
        let full = match blaze_gc::space::volume(path) {
            Ok(volume) => volume.id,
            Err(why) => {
                tracing::warn!(target: "blaze", at = %path.display(), "a build ran out of space at {}, whose volume did not measure, so it is not held for gc: {why}", path.display());
                return None;
            }
        };
        // clone: the areas, once a build out of space
        let areas = self.collector.config().areas.clone();
        let area = areas.iter().position(|area| {
            blaze_gc::space::volume(Path::new(area)).is_ok_and(|it| it.id == full)
        });
        match area {
            Some(area) => Some(self.hold(Some(area))),
            None => {
                tracing::info!(target: "blaze", at = %path.display(), "a build ran out of space at {}, on a volume the collector does not collect: it is not held", path.display());
                None
            }
        }
    }

    /// `blaze cache gc` asked of the daemon, which holds the areas (review N14): an asked run
    /// (`dry_run` plans and removes nothing) answers `to` with its report as it records
    /// ([`Gc::take_answers`]); one asked while a run of the other kind is asked waits for the
    /// next.
    pub fn ask(&mut self, to: RequestRef, dry_run: bool) {
        if !self.askers.iter().any(|(_, asked)| *asked != dry_run) {
            self.collector.ask(dry_run);
        }
        self.askers.push((to, dry_run));
    }

    /// The answers of the asked runs that recorded, for the daemon to send.
    pub fn take_answers(&mut self) -> Vec<Reply> {
        std::mem::take(&mut self.answers)
    }

    /// The askers still waiting, failed as the daemon stops.
    pub fn abandon(&mut self) -> Vec<Reply> {
        let mut out = std::mem::take(&mut self.answers);
        for (to, _) in self.askers.drain(..) {
            let error = blaze_api::ApiError::new(
                "the daemon stopped before its gc run asked for ended",
                Some("blaze cache gc".to_owned()),
            );
            out.push(Reply { to, response: Response::Error(error) });
            out.push(Reply { to, response: Response::Done { ok: false } });
        }
        out
    }

    /// The askers of `report`'s kind answered with it; the next kind asked for, if any waits.
    fn answer_askers(&mut self, report: &RunReport) {
        let mut details = None;
        let mut waiting = None;
        for (to, dry_run) in std::mem::take(&mut self.askers) {
            if dry_run != report.dry_run {
                waiting = waiting.or(Some(dry_run));
                self.askers.push((to, dry_run));
                continue;
            }
            let report = details.get_or_insert_with(|| report.details()).clone(); // clone: an asker's copy
            self.answers.push(Reply { to, response: Response::Collected { report } });
            self.answers.push(Reply { to, response: Response::Done { ok: true } });
        }
        if let Some(dry_run) = waiting {
            self.collector.ask(dry_run);
        }
    }

    /// The builds held for space that are let go, by their hold id.
    pub fn take_released(&mut self) -> Vec<(u64, Released)> {
        self.collector.take_released()
    }

    /// Each area's volume measured into the collector; one that does not measure is said and
    /// kept for status, and the collector keeps its last measure of it.
    fn measure(&mut self, now: u64) {
        self.measured_ns = now;
        // clone: the areas, once a measure
        let areas = self.collector.config().areas.clone();
        for (at, area) in areas.iter().enumerate() {
            match blaze_gc::space::volume(Path::new(area)) {
                Ok(volume) => self.collector.volume(at, volume),
                Err(why) => {
                    let why = format!(
                        "the volume of {area} was not measured, so its free space does not pace the collector: {why}"
                    );
                    // Once a change of problem: the volumes are measured every second while
                    // builds run, and the report names it once.
                    if self.problem.as_deref() != Some(why.as_str()) {
                        tracing::warn!(target: "blaze", area = %area, "{why}");
                        self.collector.note(why.clone()); // clone: the report's copy and status'
                        self.problem = Some(why);
                    }
                }
            }
        }
    }

    /// The collector's last run.
    pub fn last_run(&self) -> Option<&RunReport> {
        self.collector.last_run()
    }

    /// The loop's turn: `idle` when no job is under way, `heads` the checkouts watched now.
    /// Cheap when nothing is due: the wall clock and two counters read, nothing allocated.
    pub fn turn<'a>(&mut self, idle: bool, heads: impl Iterator<Item = &'a str> + Clone) {
        let now = blaze_gc::record::now_ns();
        let mut answered = false;
        while let Ok(back) = self.back.try_recv() {
            match back {
                Back::Answer(answer) => {
                    blaze_gc::fs::answer(&mut self.collector, answer);
                    answered = true;
                }
                Back::Scanned(answer, sizes) => {
                    self.sizes = Some(sizes);
                    blaze_gc::fs::answer(&mut self.collector, answer);
                    answered = true;
                }
                Back::Wrote(wrote) => {
                    self.writing = false;
                    if let Err(why) = wrote {
                        tracing::warn!(target: "blaze", "writing the collector's ledger: {why}");
                        self.problem = Some(why);
                    }
                    if let Some(ledger) = self.unwritten.take() {
                        self.write(ledger);
                    }
                }
            }
        }
        // A lease let go (a build ended): what it held may go, even under the urgent rate limit.
        let releases = blaze_gc::lease::releases();
        if releases != self.releases {
            self.releases = releases;
            self.collector.lease_released();
        }
        // A build that starts, and every second while builds run: one `statvfs` an area.
        let started = !idle && self.idle;
        // A build held for space is let go once the volume is measured freed (review N5).
        let every =
            if idle && !self.collector.holding() { MEASURE_IDLE_NS } else { MEASURE_BUSY_NS };
        let measured = started || now.saturating_sub(self.measured_ns) >= every;
        if measured {
            self.measure(now);
        }
        let due = self.collector.next_deadline().is_some_and(|at| now >= at);
        let ledger_due = self.ledger_due && now.saturating_sub(self.written_ns) >= LEDGER_EVERY_NS;
        if !answered
            && !measured
            && !due
            && !ledger_due
            && idle == self.idle
            && !self.collector.running()
        {
            // What was leased meanwhile matters only to a run under way.
            while blaze_gc::lease::take_used().is_some() {}
            return;
        }
        // The roots as they stand: before the mark, and before each rename is asked for.
        let known = |head: &str| self.heads.binary_search_by(|it| it.as_str().cmp(head)).is_ok();
        let same = heads.clone().count() == self.heads.len() && heads.clone().all(known);
        if !same {
            // alloc: the heads, once per change of them
            self.heads = heads.map(str::to_owned).collect();
            self.heads.sort_unstable();
            self.collector.heads(&self.heads);
            // A rename asked for before the change is not done.
            blaze_gc::lease::note_use();
        }
        // Taken before the leases below are marked: a lease taken after it stops every rename
        // asked for this turn ([`Gate::uses`]); one taken before is marked used now.
        let uses = blaze_gc::lease::uses();
        while let Some(dir) = blaze_gc::lease::take_used() {
            self.collector.used(&dir.to_string_lossy());
        }
        self.idle = idle;
        self.collector.idle(idle);
        self.collector.advance_clocks(now, blaze_gc::record::boot_ns());
        // Free space at a floor while a build runs: the build would fail for want of space.
        // The collector allows one urgent run a slow gap unless the last freed space or a
        // lease was let go since.
        if measured && !idle && self.collector.below_floor() {
            self.collector.urgent();
        }
        // Twice at most: a run that ends is measured after at once, so the builds held for it
        // are let go this turn, and what the measure starts is asked for.
        for round in 0..2 {
            let mut ended = false;
            // Any run that ended since the last drain (preempted by `urgent` above, past its
            // deadline in `advance_clocks`, or ended by an answer) stops its decisions still
            // queued on the pool before anything new is submitted (review round 6, C3).
            self.raise_ended();
            for decision in self.collector.take_decisions() {
                match decision {
                    Decision::Record { report } => {
                        self.ended.fetch_max(report.run, Ordering::SeqCst);
                        tracing::info!(target: "blaze", run = report.run, "the collector's run ended: {report}");
                        ended = true;
                        if report.why == Trigger::Asked {
                            self.answer_askers(&report);
                        }
                        if matches!(report.why, Trigger::Asked | Trigger::Urgent) {
                            self.ledger_due = false;
                            self.write(self.collector.ledger());
                        } else {
                            self.ledger_due = true;
                        }
                    }
                    decision => self.submit(decision, uses),
                }
            }
            if !ended || round == 1 {
                break;
            }
            self.measure(blaze_gc::record::now_ns());
        }
        if self.ledger_due && now.saturating_sub(self.written_ns) >= LEDGER_EVERY_NS {
            self.ledger_due = false;
            self.write(self.collector.ledger());
        }
    }

    /// [`Gate::ended`] raised to the collector's last ended run at once, before its record is
    /// drained: a run that a preempt ended has no decision start after the preempt, queued
    /// on the pool or not (review round 5, L1).
    fn raise_ended(&self) {
        self.ended.fetch_max(self.collector.ended_through(), Ordering::SeqCst);
    }

    /// `decision` done by a pool job of the class its priority names; its answer, or the panic
    /// that ended it as a failure naming the decision, sent back.
    fn submit(&mut self, decision: Decision, uses: u64) {
        let gate = Gate {
            pressure: self.collector.pressure(),
            uses,
            // The run's hard deadline; none would be no run, whose decision is not done.
            deadline_ns: self.collector.run_deadline().unwrap_or(0),
            // The deadline's own clock: a wall-clock step does not cancel the run (review N3).
            clock: self.collector.clock(),
            ended: Arc::clone(&self.ended), // clone: a refcount, once a decision
        };
        let class = class(self.collector.priority());
        if matches!(decision, Decision::Scan { .. }) {
            // An area another collector held at the last scan may be free now.
            self.lock_areas();
        }
        // clone: the job's own copy, once a decision
        let (areas, asked) = (self.held_areas.clone(), decision.clone());
        let send = self.send.clone(); // clone: a channel's sender, once a decision
        // The collector asks for one scan at a time, so the sizes are home as one is asked for;
        // were they not, this scan measures afresh, which costs time and never a wrong size.
        let sizes = match &decision {
            Decision::Scan { .. } => Some(self.sizes.take().unwrap_or_default()),
            _ => None,
        };
        blaze_graph::exec::shared().submit_as(
            None,
            class,
            move || {
                let mut sizes = sizes;
                let answer = match &mut sizes {
                    Some(sizes) => {
                        blaze_gc::fs::execute(&decision, &areas, Some(&gate), &gate.clock, sizes)
                    }
                    None => blaze_gc::fs::execute(
                        &decision,
                        &areas,
                        Some(&gate),
                        &gate.clock,
                        &mut Sizes::new(),
                    ),
                };
                (answer, sizes)
            },
            move |done| {
                let (answer, sizes) = match done {
                    Ok(it) => it,
                    Err(payload) => {
                        let why = stdx::panic_text(&*payload).unwrap_or("a panic with no message");
                        (failed(&asked, format!("the collector's job panicked: {why}")), None)
                    }
                };
                let back = match (answer, sizes) {
                    (Some(answer), Some(sizes)) => Back::Scanned(answer, sizes),
                    (Some(answer), None) => Back::Answer(answer),
                    (None, _) => return,
                };
                _ = send.send(back);
            },
        );
    }

    /// `ledger` written by a background job, one write at a time: a newer one waits for the
    /// write under way, and only the newest is written.
    fn write(&mut self, ledger: Ledger) {
        self.written_ns = blaze_gc::record::now_ns();
        if self.writing {
            self.unwritten = Some(ledger);
            return;
        }
        self.writing = true;
        let path = self.ledger.clone(); // clone: the job's own path, once a run
        let send = self.send.clone(); // clone: a channel's sender, once a run
        blaze_graph::exec::shared().submit_as(
            None,
            Class::Background,
            move || {
                let _span =
                    tracing::info_span!(target: "blaze", "gc ledger write", path = %path.display())
                        .entered();
                blaze_gc::ledger::write(&path, &ledger)
            },
            move |done| {
                let wrote = done.unwrap_or_else(|payload| {
                    let why = stdx::panic_text(&*payload).unwrap_or("a panic with no message");
                    Err(format!("the ledger's write panicked: {why}"))
                });
                _ = send.send(Back::Wrote(wrote));
            },
        );
    }
}

/// The answer of `decision` as a failure: a scan that found nothing, a rename or an unlink not
/// done, each naming `why`.
fn failed(decision: &Decision, why: String) -> Option<Answer> {
    match decision {
        Decision::Scan { run, .. } => Some(Answer::Scanned { run: *run, found: Err(why) }),
        Decision::Trash { run, path, .. } => {
            // clone: the answer's own path, once a failure
            Some(Answer::Trashed {
                run: *run,
                path: path.clone(),
                swept: blaze_gc::plan::Swept::Failed(why),
            })
        }
        // clone: the answer's own path, once a failure
        Decision::Unlink { run, path } => {
            Some(Answer::Unlinked { run: *run, path: path.clone(), done: Err(why) })
        }
        Decision::Record { .. } => None,
    }
}
