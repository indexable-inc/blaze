//! The one [`Report`]: every worker's [`Cell`] merged at exit, on one thread after the pool
//! stopped, with no lock (D179's GATHERED shape: the merge is in an order of keys, never of
//! workers or arrival, so the report is equal on 1 worker and on 128 for equal work).
//!
//! Steps: [`Report::new`], one [`Report::merge`] per worker (a second merge of a worker is
//! refused), [`Report::finish`] (folds rows by key, caps them, computes the chain), then
//! [`Report::reconcile`] against [`blaze_meter::Process`] and the sampler's profile
//! ([`Report::set_profile`]). The text form is its `Display`; the stored form is
//! [`Report::encode_into`] (`encode.rs`).

use core::{alloc::Allocator, fmt};

use blaze_meter::{Bytes, CALLS, Call, Cells, Grouped, Ns, Process, Tally, WaitRow, Waited};
use blaze_own::{
    Vec,
    planted::{NoPlant, Planted},
};

use super::{
    Brief, Cell, ChainReport, DEPTH_SAMPLES, DepthSample, Dist, EffectRow, Field, Level, Partial, Pool, SiteRow, SpanPath, SpanRow,
    TaskRec, TelemetryError, TelemetryPlant, Then, WaitDistRow, WaitRec, brief::Unit, chain::longest, sampler::Profile,
};

/// What the text says in the sampler's place when a report from `Spans` up has no profile.
/// On Linux and macOS the drivers exist (`sampler::linux`, `sampler::mac`) but the runtime does
/// not start them yet (the pool's worker start and park hooks); elsewhere there is no driver,
/// and spans and distributions are kept without samples.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub const SAMPLER_ABSENT: &str = "not started (the runtime does not start its driver yet)";
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub const SAMPLER_ABSENT: &str = "not on this target";

/// The span rows a report keeps: the 256 of most wall time, the rest folded into `other`.
pub const REPORT_SPAN_ROWS: usize = 256;

/// The wait names and allocation sites a report keeps, the rest in their `other` rows.
pub const REPORT_WAIT_ROWS: usize = 64;
pub const REPORT_SITE_ROWS: usize = 64;

/// How a report is made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ReportOpts {
    pub level: Level,
    /// The most worker cells it takes (the pool's `most_workers`, models and callers).
    pub most_workers: u32,
}

/// How the run ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Exit {
    /// `main` returned success.
    Returned,
    /// `main` returned a failure, or the run was refused.
    Failed,
    /// `main`'s own body panicked (a task's panic fails its task and is counted in its
    /// worker's `failed`, rule 20).
    Panicked,
}

impl Exit {
    pub const fn code(self) -> u64 {
        match self {
            Exit::Returned => 0,
            Exit::Failed => 1,
            Exit::Panicked => 2,
        }
    }

    pub const fn says(self) -> &'static str {
        match self {
            Exit::Returned => "returned",
            Exit::Failed => "failed",
            Exit::Panicked => "panicked",
        }
    }
}

/// One worker's row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WorkerRow {
    pub worker: u32,
    pub pool: Pool,
    pub open_spans: u64,
}

/// The report's accounting against the clock and the process.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Reconciled {
    /// The run's wall time.
    pub wall_ns: u64,
    pub workers: u32,
    /// The workers' lives, start to stop, summed.
    pub lifetime_ns: u64,
    /// Working, parked and idle, summed: equal to `lifetime_ns` when every stretch was seen.
    pub accounted_ns: u64,
    pub working_ns: u64,
    /// The process's user and system time over the run, where the system gives both.
    pub process_cpu_ns: Option<u64>,
    /// The top-level spans' user and system time (`Detail`).
    pub span_cpu_ns: Option<u64>,
    /// The process's most resident (macOS: footprint), where given.
    pub most_resident: Option<u64>,
}

/// The run's telemetry.
pub struct Report<A: Allocator + Copy> {
    pub(super) level: Level,
    pub(super) most_workers: u32,
    merged: Vec<bool, A>,
    pub(super) merges: u32,
    pub(super) cells: Cells,
    /// Every merged tally's wait rows and `other`, exact (at most `WAIT_ROWS` a worker):
    /// [`Report::finish`] gathers them into `waited` as a function of the multiset, never of
    /// the merge order (rule 25).
    wait_rows: Vec<WaitRow, A>,
    wait_other: (u64, u64),
    pub(super) waited: Waited,
    pub(super) spans: Vec<SpanRow, A>,
    pub(super) other_span: SpanRow,
    pub(super) open_spans: u64,
    pub(super) waits: Vec<WaitDistRow, A>,
    pub(super) other_wait: WaitDistRow,
    pub(super) effects: [EffectRow; CALLS],
    pub(super) effect_dists: [Dist; CALLS],
    pub(super) workers: Vec<WorkerRow, A>,
    tasks: Vec<TaskRec, A>,
    wait_recs: Vec<WaitRec, A>,
    pub(super) partial: Partial,
    pub(super) sites: Vec<SiteRow, A>,
    pub(super) other_site: SiteRow,
    pub(super) chain: Option<ChainReport>,
    pub(super) profile: Option<Profile>,
    pub(super) reconciled: Option<Reconciled>,
    pub(super) exit: Option<Exit>,
    finished: bool,
    alloc: A,
}

fn grow<T, A: Allocator>(rows: &mut Vec<T, A>, more: usize, what: &'static str) -> Result<(), TelemetryError> {
    rows.try_reserve(more).map_err(|_| TelemetryError::Reserve { what, bytes: more * size_of::<T>() })
}

impl<A: Allocator + Copy> Report<A> {
    /// An empty report; its scratch and rows are in `alloc` (the exit's).
    pub fn new(opts: ReportOpts, alloc: A) -> Result<Report<A>, TelemetryError> {
        let most = opts.most_workers as usize;
        let mut merged = Vec::try_with_capacity_in(most, alloc).map_err(|_| TelemetryError::Reserve { what: "the report's merge marks", bytes: most })?;
        for _ in 0..most {
            merged.push(false);
        }
        Ok(Report {
            level: opts.level,
            most_workers: opts.most_workers,
            merged,
            merges: 0,
            cells: Cells::ZERO,
            wait_rows: Vec::new_in(alloc),
            wait_other: (0, 0),
            waited: Waited::ZERO,
            spans: Vec::new_in(alloc),
            other_span: SpanRow::empty(SpanPath::ROOT),
            open_spans: 0,
            waits: Vec::new_in(alloc),
            other_wait: WaitDistRow::empty("other"),
            effects: [EffectRow::ZERO; CALLS],
            effect_dists: [Dist::ZERO; CALLS],
            workers: Vec::new_in(alloc),
            tasks: Vec::new_in(alloc),
            wait_recs: Vec::new_in(alloc),
            partial: Partial::default(),
            sites: Vec::new_in(alloc),
            other_site: SiteRow::empty("other"),
            chain: None,
            profile: None,
            reconciled: None,
            exit: None,
            finished: false,
            alloc,
        })
    }

    /// Worker `cell.worker()`'s cell and its `tally`, merged. Refused for a worker merged
    /// already, past `most_workers`, or kept at another level.
    pub fn merge<B: Allocator + Copy>(&mut self, tally: &Tally, cell: &Cell<B>) -> Result<(), TelemetryError> {
        self.merge_planted(tally, cell, NoPlant)
    }

    pub fn merge_planted<B: Allocator + Copy>(
        &mut self,
        tally: &Tally,
        cell: &Cell<B>,
        plant: impl Planted<TelemetryPlant>,
    ) -> Result<(), TelemetryError> {
        let worker = cell.worker();
        if cell.level() != self.level {
            return Err(TelemetryError::LevelDiffers { cell: cell.level(), report: self.level });
        }
        let Some(mark) = self.merged.get_mut(worker as usize) else {
            return Err(TelemetryError::WorkerPastMost { worker, most: self.most_workers });
        };
        if *mark && !plant.has(TelemetryPlant::MergedTwice) {
            return Err(TelemetryError::MergedTwice { worker });
        }
        *mark = true;
        self.merges += 1;
        self.finished = false;
        self.cells = self.cells.plus(tally.cells());
        let waits_of = tally.waits();
        grow(&mut self.wait_rows, waits_of.rows.len(), "the report's wait rows")?;
        for row in waits_of.rows.iter().flatten() {
            self.wait_rows.push(*row);
        }
        self.wait_other = (self.wait_other.0.wrapping_add(waits_of.other_count), self.wait_other.1.wrapping_add(waits_of.other_ns));
        let (spans, waits, effects, pool, chain, sites) = cell.parts();
        self.open_spans = self.open_spans.wrapping_add(cell.open_spans());
        let rows: usize = spans.rows().count();
        grow(&mut self.spans, rows, "the report's spans")?;
        for row in spans.rows() {
            self.spans.push(*row);
        }
        self.other_span.join(spans.other());
        grow(&mut self.waits, waits.rows().len(), "the report's waits")?;
        for row in waits.rows() {
            self.waits.push(*row);
        }
        self.other_wait.join(waits.other());
        for (at, row) in effects.rows().iter().enumerate() {
            self.effects[at].join(row);
        }
        for (at, dist) in effects.dists().iter().enumerate() {
            self.effect_dists[at].plus(dist);
        }
        grow(&mut self.workers, 1, "the report's workers")?;
        self.workers.push(WorkerRow { worker, pool: *pool, open_spans: cell.open_spans() });
        grow(&mut self.tasks, chain.tasks().len(), "the report's chain tasks")?;
        self.tasks.extend_from_slice(chain.tasks());
        grow(&mut self.wait_recs, chain.waits().len(), "the report's chain waits")?;
        self.wait_recs.extend_from_slice(chain.waits());
        let past = chain.partial();
        self.partial.tasks_past += past.tasks_past;
        self.partial.waits_past += past.waits_past;
        grow(&mut self.sites, sites.rows().len(), "the report's sites")?;
        for row in sites.rows() {
            self.sites.push(*row);
        }
        self.other_site.join(sites.other());
        Ok(())
    }

    /// Rows folded by key and capped, the workers in index order, the chain computed; `exit`
    /// is how the run ended.
    pub fn finish(&mut self, exit: Exit) -> Result<(), TelemetryError> {
        self.exit = Some(exit);
        self.workers.sort_unstable_by_key(|row| row.worker);
        self.waited = Waited::gathered(&mut self.wait_rows, self.wait_other.0, self.wait_other.1);
        // Spans: by path, equal paths joined; then the most wall time kept.
        self.spans.sort_unstable_by(|a, b| a.path.order(&b.path));
        fold(&mut self.spans, |a, b| a.path.same(&b.path), SpanRow::join);
        if self.spans.len() > REPORT_SPAN_ROWS {
            self.spans.sort_unstable_by(|a, b| b.wall_ns.cmp(&a.wall_ns).then(a.path.order(&b.path)));
            for row in &self.spans[REPORT_SPAN_ROWS..] {
                self.other_span.join(row);
            }
            self.spans.truncate(REPORT_SPAN_ROWS);
            self.spans.sort_unstable_by(|a, b| a.path.order(&b.path));
        }
        self.waits.sort_unstable_by(|a, b| a.on.cmp(b.on));
        fold(&mut self.waits, |a, b| a.on == b.on, WaitDistRow::join);
        if self.waits.len() > REPORT_WAIT_ROWS {
            self.waits.sort_unstable_by(|a, b| b.ns.cmp(&a.ns).then(a.on.cmp(b.on)));
            for row in &self.waits[REPORT_WAIT_ROWS..] {
                self.other_wait.join(row);
            }
            self.waits.truncate(REPORT_WAIT_ROWS);
            self.waits.sort_unstable_by(|a, b| a.on.cmp(b.on));
        }
        self.sites.sort_unstable_by(|a, b| a.site.cmp(b.site));
        fold(&mut self.sites, |a, b| a.site == b.site, SiteRow::join);
        if self.sites.len() > REPORT_SITE_ROWS {
            self.sites.sort_unstable_by(|a, b| b.bytes.cmp(&a.bytes).then(a.site.cmp(b.site)));
            for row in &self.sites[REPORT_SITE_ROWS..] {
                self.other_site.join(row);
            }
            self.sites.truncate(REPORT_SITE_ROWS);
            self.sites.sort_unstable_by(|a, b| a.site.cmp(b.site));
        }
        // The chain's records in an order of keys, so the walk's ties fall the same way.
        self.tasks.sort_unstable_by(|a, b| a.task.cmp(&b.task).then(a.start.cmp(&b.start)));
        self.wait_recs.sort_unstable_by(|a, b| a.task.cmp(&b.task).then(a.start.cmp(&b.start)).then(a.end.cmp(&b.end)).then(a.on.cmp(b.on)));
        self.chain = if self.level.keeps(Level::Spans) { longest(&self.tasks, &self.wait_recs, self.partial, self.alloc)? } else { None };
        self.finished = true;
        Ok(())
    }

    /// The report reconciled: `wall_ns` the run's wall time, `begun` and `ended` the process's
    /// totals at the run's start and now.
    pub fn reconcile(&mut self, wall_ns: u64, begun: &Process, ended: &Process) -> Result<(), TelemetryError> {
        if !self.finished {
            return Err(TelemetryError::Unfinished);
        }
        let mut lifetime_ns = 0u64;
        let mut accounted_ns = 0u64;
        let mut working_ns = 0u64;
        for row in self.workers.iter() {
            lifetime_ns = lifetime_ns.wrapping_add(row.pool.lifetime().unwrap_or(0));
            accounted_ns = accounted_ns.wrapping_add(row.pool.accounted());
            working_ns = working_ns.wrapping_add(row.pool.working_ns);
        }
        let used = ended.system.since(begun.system);
        let process_cpu_ns = match (used.user_ns.got(), used.system_ns.got()) {
            (Some(user), Some(system)) => Some(user.wrapping_add(system)),
            _ => None,
        };
        let span_cpu_ns = if self.level.keeps(Level::Detail) {
            let mut sum = 0u64;
            for row in self.spans.iter().filter(|row| row.path.depth() == 1) {
                sum = sum.wrapping_add(row.sum.get(Field::UserNs)).wrapping_add(row.sum.get(Field::SystemNs));
            }
            Some(sum)
        } else {
            None
        };
        self.reconciled = Some(Reconciled {
            wall_ns,
            workers: self.merges,
            lifetime_ns,
            accounted_ns,
            working_ns,
            process_cpu_ns,
            span_cpu_ns,
            most_resident: ended.most_resident.got(),
        });
        Ok(())
    }

    /// The sampler's profile ([`super::sampler::fold`]), folded after [`Report::finish`].
    pub fn set_profile(&mut self, profile: Profile) {
        self.profile = Some(profile);
    }

    pub fn level(&self) -> Level {
        self.level
    }

    pub fn merges(&self) -> u32 {
        self.merges
    }

    pub fn finished(&self) -> bool {
        self.finished
    }

    pub fn cells(&self) -> &Cells {
        &self.cells
    }

    pub fn waited(&self) -> &Waited {
        &self.waited
    }

    pub fn spans(&self) -> &[SpanRow] {
        &self.spans
    }

    pub fn other_span(&self) -> &SpanRow {
        &self.other_span
    }

    pub fn open_spans(&self) -> u64 {
        self.open_spans
    }

    pub fn waits(&self) -> &[WaitDistRow] {
        &self.waits
    }

    pub fn other_wait(&self) -> &WaitDistRow {
        &self.other_wait
    }

    pub fn effects(&self) -> &[EffectRow; CALLS] {
        &self.effects
    }

    pub fn effect_dists(&self) -> &[Dist; CALLS] {
        &self.effect_dists
    }

    pub fn workers(&self) -> &[WorkerRow] {
        &self.workers
    }

    pub fn sites(&self) -> &[SiteRow] {
        &self.sites
    }

    pub fn chain(&self) -> Option<&ChainReport> {
        self.chain.as_ref()
    }

    pub fn profile(&self) -> Option<&Profile> {
        self.profile.as_ref()
    }

    pub fn reconciled(&self) -> Option<&Reconciled> {
        self.reconciled.as_ref()
    }

    pub fn exit(&self) -> Option<Exit> {
        self.exit
    }

    /// The last [`DEPTH_SAMPLES`] ready-queue samples of all workers, by time then worker.
    pub fn depth_samples(&self, into: &mut Vec<DepthSample, A>) -> Result<(), TelemetryError> {
        into.clear();
        let all: usize = self.workers.iter().map(|row| row.pool.samples().count()).sum();
        grow(into, all, "the report's depth samples")?;
        for row in self.workers.iter() {
            for sample in row.pool.samples() {
                into.push(*sample);
            }
        }
        into.sort_unstable_by(|a, b| a.at_ns.cmp(&b.at_ns).then(a.worker.cmp(&b.worker)));
        if into.len() > DEPTH_SAMPLES {
            let drop = into.len() - DEPTH_SAMPLES;
            into.drain(..drop);
        }
        Ok(())
    }

    pub(super) fn alloc(&self) -> A {
        self.alloc
    }
}

/// Adjacent rows that are `same` joined into the first.
fn fold<T: Copy, A: Allocator>(rows: &mut Vec<T, A>, same: impl Fn(&T, &T) -> bool, join: impl Fn(&mut T, &T)) {
    if rows.is_empty() {
        return;
    }
    let mut kept = 0;
    for at in 1..rows.len() {
        let row = rows[at];
        if same(&rows[kept], &row) {
            join(&mut rows[kept], &row);
        } else {
            kept += 1;
            rows[kept] = row;
        }
    }
    rows.truncate(kept + 1);
}

/// `", "` before every part but the first.
struct Parts {
    first: bool,
}

impl Parts {
    fn sep(&mut self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if core::mem::replace(&mut self.first, false) { Ok(()) } else { f.write_str(", ") }
    }
}

/// A distribution's median, p99 and longest, as brackets.
struct Shape<'a>(&'a Dist);

impl fmt::Display for Shape<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (Some(median), Some(p99), Some(longest)) = (self.0.median(), self.0.p99(), self.0.longest()) else {
            return Ok(());
        };
        write!(
            f,
            " (median {}..{}, p99 {}..{}, longest {}..{})",
            Ns(median.low),
            Ns(median.high),
            Ns(p99.low),
            Ns(p99.high),
            Ns(longest.low),
            Ns(longest.high)
        )
    }
}

/// A brief's non-zero fields.
struct Fields<'a>(&'a Brief);

impl fmt::Display for Fields<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Parts { first: true };
        for field in Field::ALL {
            let n = self.0.get(field);
            if n == 0 || matches!(field, Field::Waits | Field::Calls) {
                continue;
            }
            parts.sep(f)?;
            match field.unit() {
                Unit::Ns => write!(f, "{} {}", field.name(), Ns(n))?,
                Unit::Bytes => write!(f, "{} {}", field.name(), Bytes(n))?,
                Unit::Count => write!(f, "{} {}", Grouped(n), field.name())?,
            }
        }
        Ok(())
    }
}

struct Path<'a>(&'a SpanPath);

impl fmt::Display for Path<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names = self.0.names();
        if names.is_empty() {
            return f.write_str("(other)");
        }
        for (at, name) in names.iter().enumerate() {
            if at > 0 {
                f.write_str("/")?;
            }
            f.write_str(name)?;
        }
        if self.0.deeper() {
            f.write_str("/…")?;
        }
        Ok(())
    }
}

/// The text form: compact, in a fixed order, non-zero rows only, in the style of the meter's
/// `used:` lines. A report not finished prints that it is not.
impl<A: Allocator + Copy> fmt::Display for Report<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.finished {
            return f.write_str("telemetry: not finished");
        }
        write!(f, "telemetry ({}): {} workers", self.level.name(), self.merges)?;
        if let Some(exit) = self.exit {
            write!(f, ", {}", exit.says())?;
        }
        if self.open_spans > 0 {
            write!(f, ", {} spans OPEN at exit", Grouped(self.open_spans))?;
        }
        writeln!(f)?;
        if let Some(r) = &self.reconciled {
            write!(
                f,
                "reconciled: wall {}, workers' lives {}: working {}, accounted {}",
                Ns(r.wall_ns),
                Ns(r.lifetime_ns),
                Ns(r.working_ns),
                Ns(r.accounted_ns)
            )?;
            if r.accounted_ns != r.lifetime_ns {
                write!(f, " (UNACCOUNTED {})", Ns(r.lifetime_ns.abs_diff(r.accounted_ns)))?;
            }
            if let Some(cpu) = r.process_cpu_ns {
                write!(f, "; process cpu {}", Ns(cpu))?;
            }
            if let Some(cpu) = r.span_cpu_ns {
                write!(f, ", spans' cpu {}", Ns(cpu))?;
            }
            if let Some(most) = r.most_resident {
                write!(f, ", most resident {}", Bytes(most))?;
            }
            writeln!(f)?;
        }
        for row in self.workers.iter() {
            let p = &row.pool;
            if p.tasks == 0 && p.working_ns == 0 {
                continue;
            }
            write!(f, "worker {}: working {}, parked {}, idle {}, {} tasks", row.worker, Ns(p.working_ns), Ns(p.parked_ns), Ns(p.idle_ns), Grouped(p.tasks))?;
            if p.failed > 0 {
                write!(f, " ({} FAILED)", Grouped(p.failed))?;
            }
            if let Some(longest) = p.longest {
                write!(f, ", longest `{}` {}", longest.name, Ns(longest.ns))?;
            }
            writeln!(f)?;
        }
        let totals = Brief::of(&self.cells, &self.waited, None);
        writeln!(f, "used: {}", Fields(&totals))?;
        // Effects, syncs on their own line after the rest.
        for pass in [false, true] {
            for call in Call::ALL {
                if (call == Call::Fsync) != pass {
                    continue;
                }
                let row = &self.effects[call as usize];
                if row.count == 0 {
                    continue;
                }
                let what = if pass { "sync" } else { "effect" };
                write!(f, "{what} {}: {} {}", call.name(), Grouped(row.count), Ns(row.ns))?;
                if row.bytes > 0 {
                    write!(f, " {}", Bytes(row.bytes))?;
                }
                write!(f, "{}", Shape(&self.effect_dists[call as usize]))?;
                writeln!(f)?;
            }
        }
        for row in self.waits.iter().chain(core::iter::once(&self.other_wait)) {
            if row.count == 0 {
                continue;
            }
            writeln!(f, "wait `{}`: {} {}{}", row.on, Grouped(row.count), Ns(row.ns), Shape(&row.dist))?;
        }
        for row in self.spans.iter().chain(core::iter::once(&self.other_span)) {
            if row.count == 0 {
                continue;
            }
            writeln!(f, "span {}: {}× {} (most {}), {}", Path(&row.path), Grouped(row.count), Ns(row.wall_ns), Ns(row.wall_most), Fields(&row.sum))?;
        }
        if let Some(chain) = &self.chain {
            write!(f, "chain {}:", Ns(chain.total_ns))?;
            for link in chain.links.iter().flatten() {
                write!(f, " `{}` {}", link.name, Ns(link.own_ns))?;
                match link.then {
                    Then::Nothing => {}
                    Then::Task(_) => f.write_str(" <-")?,
                    Then::Wait { on, ns } => write!(f, " <- wait `{on}` {}", Ns(ns))?,
                }
            }
            if chain.len as usize > chain.links.len() {
                write!(f, " … ({} links)", chain.len)?;
            }
            if chain.partial.tasks_past > 0 || chain.partial.waits_past > 0 {
                write!(f, " (partial: {} tasks and {} waits past the caps)", Grouped(chain.partial.tasks_past), Grouped(chain.partial.waits_past))?;
            }
            writeln!(f)?;
        }
        for row in self.sites.iter().chain(core::iter::once(&self.other_site)) {
            if row.count == 0 {
                continue;
            }
            write!(f, "site `{}`: {} allocs {}", row.site, Grouped(row.count), Bytes(row.bytes))?;
            if let Some(median) = row.sizes.median() {
                write!(f, " (median {}..{})", Bytes(median.low), Bytes(median.high))?;
            }
            writeln!(f)?;
        }
        match &self.profile {
            Some(profile) => write!(f, "{profile}")?,
            None if self.level.keeps(Level::Spans) => writeln!(f, "sampler: {SAMPLER_ABSENT}")?,
            None => {}
        }
        Ok(())
    }
}
