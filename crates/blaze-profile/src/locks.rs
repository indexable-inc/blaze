//! The named locks' waits (blaze_core::lock, docs/v2/plan/design/named-locks.md), in the
//! profile: [`Recorder`] is the process's [`Probe`], which every contended acquire of a
//! `blaze_core::lock::{Lock, ReadLock}` reports to, and every `Signal` wait opens its span
//! through; each wait is one event of the target [`TARGET`] (the `lock_waits` view of
//! sql/views.sql); and [`rank`] reads a build's rows back as `blaze debug locks` says them: each
//! lock by its total wait, where it was waited for, behind whom, and what to use instead.
//!
//! The probe is called only on a slow path (a lock someone held, a signal's wait), never while
//! an uncontended lock is taken or let go. A lock on its own path (the flight recorder's) must
//! be `Lock::unrecorded`: recording its wait would take it again.

use std::{collections::BTreeMap, fmt, fmt::Write as _, panic::Location, path::Path};

use arrow_array::RecordBatch;
use blaze_core::lock::{Awaits, LockWait, Probe};
use blaze_graph::wait::WaitKind;
use serde_derive::Serialize;

use crate::{
    clock, flight,
    gate::{self, GateError},
    sql::{self, SqlError},
};

/// The target of a lock wait's event: what the `lock_waits` view reads. INFO of a `blaze` target
/// is kept by the recorder (`logs::keeps_event`) and written by the history.
pub const TARGET: &str = "blaze::lock";
/// The message of a lock wait's event.
pub const MESSAGE: &str = "lock_wait";
/// The target of a stall's event (the daemon's watchdog, blaze_core::lock::stall): what the
/// `stalls` view reads.
pub const STALL_TARGET: &str = "blaze::stall";

/// The profiler as the locks' [`Probe`]: its clock, its span and thread of the caller, its
/// `wait` span, and an event a wait.
pub struct Recorder;

/// What [`crate::install`] hands to `blaze_core::lock::install`.
pub static RECORDER: &dyn Probe = &Recorder;

impl Probe for Recorder {
    /// The profile's clock ([`clock::mono_ns`], every instant of the schema's), so a wait's
    /// `start_ns` lines up with `spans.start_ns`.
    fn now_ns(&self) -> u64 {
        // The machine's monotonic clock is never before its zero.
        clock::mono_ns() as u64
    }

    fn span(&self) -> u64 {
        flight::span_now()
    }

    /// The operating system's thread id, as every row of the profile names a thread (and as
    /// `flight` marks the thread in the lock module's wait-for graph when it claims its ring).
    fn thread(&self) -> u64 {
        u64::from(crate::sampler::os_tid() as u32)
    }

    /// The same `wait` span the scheduler's waits open (`pump::span_waits`): `awaits` is `lock
    /// <name>` (kind lock) or `signal <name>` (kind channel).
    fn wait_begin(&self, awaits: Awaits) -> u64 {
        let (kind, said, name) = match awaits {
            Awaits::Lock(name, _) => (WaitKind::Lock, "lock", name),
            Awaits::Signal(name) => (WaitKind::Channel, "signal", name),
        };
        // alloc: once a contended wait, its span's `awaits` (interned by the span)
        let awaited = format!("{said} {name}");
        crate::wait_began(kind, &awaited, None)
    }

    fn wait_end(&self, token: u64) {
        crate::wait_ended(token);
    }

    fn record(&self, wait: &LockWait) {
        record(wait);
    }
}

/// A call site as `file:line`; nothing for none.
struct At(Option<&'static Location<'static>>);

impl fmt::Display for At {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(it) => write!(f, "{}:{}", it.file(), it.line()),
            None => Ok(()),
        }
    }
}

/// One wait, as one event of [`TARGET`] named [`MESSAGE`]: a row of `events` and its fields'
/// rows of `event_fields`, which the `lock_waits` view puts back together. A wait behind no
/// holder that said itself has the `holder_*` fields 0 and "".
///
/// THE SEAM: this is the one place a lock wait becomes a row. Today it goes through the flight
/// recorder's event path, and the per-thread buffers (branch `prof-per-thread`,
/// `blaze_profile::flight::names`) already put per-thread rows through `flight::Ring::put`;
/// moving lock waits to a row kind of their own is a change to this one function (and the view).
pub fn record(wait: &LockWait) {
    let holder = wait.holder;
    tracing::info!(
        target: "blaze::lock",
        lock = wait.lock,
        value_type = wait.value_type,
        mode = wait.mode.name(),
        site = %At(Some(wait.site)),
        span = wait.span,
        thread = wait.thread,
        start_ns = wait.start_ns,
        wait_ns = wait.wait_ns,
        holder_site = %At(holder.map(|it| it.site)),
        holder_span = holder.map_or(0u64, |it| it.span),
        holder_thread = holder.map_or(0u64, |it| it.thread),
        holder_mode = holder.map_or("", |it| it.mode.name()),
        hold_ns = holder.map_or(0u64, |it| it.hold_ns),
        hold_whole = holder.is_some_and(|it| it.whole),
        "lock_wait"
    );
}

// --- Reading them back: `blaze debug locks` ----------------------------------------------------

/// One row of `lock_waits`, with its waiter's and holder's span names (`spans.name`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Row {
    pub lock: String,
    pub value_type: String,
    /// `lock`, `read` or `write`.
    pub mode: String,
    pub site: String,
    pub thread: u64,
    pub wait_ns: u64,
    /// "" when no holder said itself.
    pub holder_mode: String,
    /// 0 when no holder said itself.
    pub holder_thread: u64,
    /// The waiter's span's name, "" for none (or one with no row).
    pub waiter_span: String,
    pub holder_span: String,
}

/// A call site of a lock, by its waits there.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Site {
    pub site: String,
    pub wait_ns: u64,
    pub waits: u64,
}

/// One lock of a build, ranked by its total wait.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Ranked {
    pub lock: String,
    /// The type it guards, as its most-waited rows name it.
    pub value_type: String,
    pub wait_ns: u64,
    pub waits: u64,
    /// Its wait as a share of the build's pool-worker time, in percent; `None` with no workers.
    pub worker_pct: Option<f64>,
    /// Its call sites, the most waited at first, at most [`SITES_SHOWN`].
    pub sites: Vec<Site>,
    /// The span name its waiters waited in most (by wait), and its holders held it in most.
    pub top_waiter: Option<String>,
    pub top_holder: Option<String>,
    /// What to use instead, read from its rows ([`recommend`]).
    pub instead: &'static str,
}

/// The call sites a ranked lock lists.
pub const SITES_SHOWN: usize = 3;

pub const ARC_SWAP: &str = "arc-swap or RCU";
pub const MAP: &str = "papaya::HashMap (shared) or blaze_hash::HashMap behind one owner";
pub const ATOMIC: &str = "an atomic";
pub const CHANNEL: &str = "a crossbeam channel";
pub const OWNER: &str = "move it into its owner and send it messages";
pub const SHORTER: &str = "shorten the critical section";

/// The types an atomic holds.
const ATOMIC_TYPES: [&str; 13] = [
    "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128", "isize", "bool",
];

/// What to use in place of a lock guarding `value_type`, from its waits `rows`, by the first rule
/// that holds:
/// - reads are 80% or more of the waits and most holders that said themselves wrote: arc-swap
///   or RCU;
/// - a map or set: `papaya::HashMap`, or `blaze_hash::HashMap` behind one owner;
/// - an integer or bool: an atomic;
/// - a `VecDeque` or `Vec` that waiters on two threads or more wait for: a crossbeam channel;
/// - every holder that said itself is one thread: move the value into that owner;
/// - else: shorten the critical section.
pub fn recommend(value_type: &str, rows: &[&Row]) -> &'static str {
    let reads = rows.iter().filter(|it| it.mode == "read").count();
    let held = || rows.iter().filter(|it| it.holder_thread != 0);
    let (said, writes) = (held().count(), held().filter(|it| it.holder_mode == "write").count());
    if !rows.is_empty() && reads * 5 >= rows.len() * 4 && writes * 2 > said {
        return ARC_SWAP;
    }
    if ["HashMap", "BTreeMap", "HashSet", "IndexMap"].iter().any(|it| value_type.contains(it)) {
        return MAP;
    }
    if ATOMIC_TYPES.iter().any(|&it| it == value_type.trim()) {
        return ATOMIC;
    }
    let mut waiters = rows.iter().map(|it| it.thread);
    let first = waiters.next();
    if value_type.contains("Vec") && waiters.any(|it| Some(it) != first) {
        return CHANNEL;
    }
    let mut holders = held().map(|it| it.holder_thread);
    if let Some(one) = holders.next()
        && holders.all(|it| it == one)
    {
        return OWNER;
    }
    SHORTER
}

/// The key of `rows` (by `of`) with the most wait, ties to the least key; `None` for an empty
/// key only.
fn most_waited<'a>(rows: &[&'a Row], of: impl Fn(&'a Row) -> &'a str) -> Option<&'a str> {
    // alloc: a report's grouping, once a lock
    let mut by: BTreeMap<&str, u64> = BTreeMap::new();
    for &row in rows {
        *by.entry(of(row)).or_default() += row.wait_ns;
    }
    by.into_iter()
        .filter(|(key, _)| !key.is_empty())
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(key, _)| key)
}

/// `rows`' locks, the most waited for first (ties by name), against `worker_ns`, the build's
/// pool-worker time (0: none). Pure: the same rows in any order give the same ranking.
pub fn rank(rows: &[Row], worker_ns: u64) -> Vec<Ranked> {
    // Ordered, so the output is.
    let mut by: BTreeMap<&str, Vec<&Row>> = BTreeMap::new(); // alloc: a report's grouping
    for row in rows {
        by.entry(row.lock.as_str()).or_default().push(row);
    }
    let mut ranked: Vec<Ranked> = by
        .into_iter()
        .map(|(lock, rows)| {
            let wait_ns: u64 = rows.iter().map(|it| it.wait_ns).sum();
            let mut sites: BTreeMap<&str, Site> = BTreeMap::new(); // alloc: a report's grouping
            for row in &rows {
                let site = sites.entry(row.site.as_str()).or_insert_with(|| Site {
                    // alloc: a report's line, once a site
                    site: row.site.clone(),
                    wait_ns: 0,
                    waits: 0,
                });
                site.wait_ns += row.wait_ns;
                site.waits += 1;
            }
            // alloc: a report's lines, once a lock
            let mut sites: Vec<Site> = sites.into_values().collect();
            sites.sort_by(|a, b| b.wait_ns.cmp(&a.wait_ns).then_with(|| a.site.cmp(&b.site)));
            sites.truncate(SITES_SHOWN);
            let value_type = most_waited(&rows, |it| it.value_type.as_str()).unwrap_or("");
            Ranked {
                // alloc: a report's line, once a lock
                lock: lock.to_owned(),
                // alloc: as above
                value_type: value_type.to_owned(),
                wait_ns,
                waits: rows.len() as u64,
                worker_pct: (worker_ns > 0).then(|| 100.0 * wait_ns as f64 / worker_ns as f64),
                sites,
                // alloc: as above
                top_waiter: most_waited(&rows, |it| it.waiter_span.as_str()).map(str::to_owned),
                // alloc: as above
                top_holder: most_waited(&rows, |it| it.holder_span.as_str()).map(str::to_owned),
                instead: recommend(value_type, &rows),
            }
        })
        // alloc: a report, once a report
        .collect();
    ranked.sort_by(|a, b| b.wait_ns.cmp(&a.wait_ns).then_with(|| a.lock.cmp(&b.lock)));
    ranked
}

/// One row of the `stalls` view.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Stalled {
    pub t_ns: i64,
    /// `deadlock` or `convoy`.
    pub kind: String,
    pub line: String,
    pub head: u64,
    pub waiters: u64,
    pub threads: String,
    pub oldest_ns: u64,
}

/// `blaze debug locks`'s answer: a build's locks ranked by wait, and its stalls.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Report {
    pub build: u64,
    /// The build's pool workers times its wall time, as the gate's `workers_covered` counts it.
    pub worker_ns: u64,
    /// Every lock waited for in the build, and every wait.
    pub locks_waited: u64,
    pub waits: u64,
    pub wait_ns: u64,
    /// The first `top` of them ([`load`]).
    pub locks: Vec<Ranked>,
    pub stalls: Vec<Stalled>,
}

/// Why `blaze debug locks` has no report.
#[derive(Debug)]
pub enum LocksError {
    Sql(SqlError),
    /// The history has no ended build `build` (`None`: none at all).
    NoBuild { history: std::path::PathBuf, build: Option<u64> },
    /// DuckDB answered columns other than those asked for: a bug in the query here.
    Answer(String),
}

impl fmt::Display for LocksError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LocksError::Sql(error) => write!(f, "blaze debug locks: {error}"),
            LocksError::NoBuild { history, build: Some(build) } => {
                write!(f, "blaze debug locks: {} has no ended build {build}", history.display())
            }
            LocksError::NoBuild { history, build: None } => {
                write!(f, "blaze debug locks: {} has no build that ended", history.display())
            }
            LocksError::Answer(why) => write!(f, "blaze debug locks: the lock_waits query: {why}"),
        }
    }
}

impl std::error::Error for LocksError {}

impl From<GateError> for LocksError {
    fn from(error: GateError) -> LocksError {
        match error {
            GateError::Sql(error) => LocksError::Sql(error),
            GateError::NoBuild { history, build } => LocksError::NoBuild { history, build },
            // alloc: an error
            GateError::Summary { why, .. } => LocksError::Answer(why),
        }
    }
}

/// `build`'s (the last that ended, for `None`) locks, ranked ([`rank`]), the first `top` kept,
/// and its stalls, from the history at `history` read by the DuckDB at `duckdb`.
pub fn load(
    duckdb: &Path,
    history: &Path,
    build: Option<u64>,
    top: usize,
) -> Result<Report, LocksError> {
    let it = gate::build(duckdb, history, build)?;
    let (events, workers) = (it.events(), it.workers());
    // The views read a build's files; the waiter's and holder's spans are its process's.
    // alloc: a report's queries, once a report
    let rows = format!(
        "SELECT CAST(coalesce(w.lock, '?') AS VARCHAR), CAST(coalesce(w.value_type, '') AS VARCHAR), \
         CAST(coalesce(w.mode, '') AS VARCHAR), CAST(coalesce(w.site, '') AS VARCHAR), \
         CAST(w.thread AS BIGINT), CAST(w.wait_ns AS BIGINT), CAST(w.holder_mode AS VARCHAR), \
         CAST(w.holder_thread AS BIGINT), CAST(coalesce(s.name, '') AS VARCHAR), \
         CAST(coalesce(h.name, '') AS VARCHAR) \
         FROM (SELECT * FROM lock_waits WHERE {events}) AS w \
         LEFT JOIN spans AS s ON s.process = w.process AND s.span_id = w.span AND w.span <> 0 \
         LEFT JOIN spans AS h ON h.process = w.process AND h.span_id = w.holder_span AND w.holder_span <> 0 \
         ORDER BY w.process, w.event_id"
    );
    // alloc: as above
    let workers = format!("SELECT CAST(count(*) AS BIGINT) FROM {workers} AS w");
    // alloc: as above
    let stalls = format!(
        "SELECT CAST(t_ns AS BIGINT), CAST(coalesce(kind, '') AS VARCHAR), CAST(coalesce(line, '') AS VARCHAR), \
         CAST(head AS BIGINT), CAST(waiters AS BIGINT), CAST(coalesce(threads, '') AS VARCHAR), \
         CAST(oldest_ns AS BIGINT) FROM stalls WHERE {events} ORDER BY t_ns, process, event_id"
    );
    let answers = sql::run_all(
        duckdb,
        history,
        sql::Scope::Build(it.build),
        &[rows.as_str(), workers.as_str(), stalls.as_str()],
    )
    .map_err(LocksError::Sql)?;
    let [rows, workers, stalls] = answers.as_slice() else {
        // alloc: an error
        return Err(LocksError::Answer(format!("DuckDB gave {} answers to 3 queries", answers.len())));
    };
    let rows = read_rows(rows).map_err(LocksError::Answer)?;
    let workers = gate::i64s(workers, 0).map_err(LocksError::Answer)?;
    let worker_ns =
        workers.first().map_or(0, |&n| u64::try_from(n * (it.t1 - it.t0)).unwrap_or(0));
    let mut locks = rank(&rows, worker_ns);
    let locks_waited = locks.len() as u64;
    locks.truncate(top);
    Ok(Report {
        build: it.build,
        worker_ns,
        locks_waited,
        waits: rows.len() as u64,
        wait_ns: rows.iter().map(|it| it.wait_ns).sum(),
        locks,
        stalls: read_stalls(stalls).map_err(LocksError::Answer)?,
    })
}

fn unsigned(it: Vec<i64>) -> impl Iterator<Item = u64> {
    it.into_iter().map(|it| u64::try_from(it).unwrap_or(0))
}

fn read_rows(batches: &[RecordBatch]) -> Result<Vec<Row>, String> {
    let s = |at| gate::strings(batches, at);
    let (lock, value_type, mode, site) = (s(0)?, s(1)?, s(2)?, s(3)?);
    let (thread, wait_ns) = (gate::i64s(batches, 4)?, gate::i64s(batches, 5)?);
    let (holder_mode, holder_thread) = (s(6)?, gate::i64s(batches, 7)?);
    let (waiter_span, holder_span) = (s(8)?, s(9)?);
    let mut thread = unsigned(thread);
    let mut wait_ns = unsigned(wait_ns);
    let mut holder_thread = unsigned(holder_thread);
    let mut rest = value_type.into_iter().zip(mode).zip(site).zip(holder_mode);
    let mut spans = waiter_span.into_iter().zip(holder_span);
    // alloc: a report's rows, once a report
    let mut out = Vec::with_capacity(lock.len());
    for lock in lock {
        let (Some((((value_type, mode), site), holder_mode)), Some((waiter_span, holder_span))) =
            (rest.next(), spans.next())
        else {
            break;
        };
        out.push(Row {
            lock,
            value_type,
            mode,
            site,
            thread: thread.next().unwrap_or(0),
            wait_ns: wait_ns.next().unwrap_or(0),
            holder_mode,
            holder_thread: holder_thread.next().unwrap_or(0),
            waiter_span,
            holder_span,
        });
    }
    Ok(out)
}

fn read_stalls(batches: &[RecordBatch]) -> Result<Vec<Stalled>, String> {
    let (t_ns, kind, line) =
        (gate::i64s(batches, 0)?, gate::strings(batches, 1)?, gate::strings(batches, 2)?);
    let (head, waiters) = (unsigned(gate::i64s(batches, 3)?), unsigned(gate::i64s(batches, 4)?));
    let (threads, oldest) = (gate::strings(batches, 5)?, unsigned(gate::i64s(batches, 6)?));
    Ok(t_ns
        .into_iter()
        .zip(kind)
        .zip(line)
        .zip(head)
        .zip(waiters)
        .zip(threads)
        .zip(oldest)
        .map(|((((((t_ns, kind), line), head), waiters), threads), oldest_ns)| Stalled {
            t_ns,
            kind,
            line,
            head,
            waiters,
            threads,
            oldest_ns,
        })
        // alloc: a report's rows, once a report
        .collect())
}

fn ms(ns: u64) -> f64 {
    ns as f64 / 1e6
}

/// `report` as `blaze debug locks` prints it.
pub fn text(report: &Report) -> String {
    // alloc: a report's text, once a report
    let mut out = String::new();
    // Writing to a `String` does not fail.
    _ = writeln!(
        out,
        "build {}: {} named locks waited for {:.1} ms over {} waits, of {:.1} ms of pool-worker time",
        report.build,
        report.locks_waited,
        ms(report.wait_ns),
        report.waits,
        ms(report.worker_ns)
    );
    for (i, it) in report.locks.iter().enumerate() {
        let share = it.worker_pct.map_or_else(
            || "no pool worker to weigh against".to_owned(), // alloc: a report's line
            |pct| format!("{pct:.2}% of worker time"),       // alloc: as above
        );
        _ = writeln!(
            out,
            "{:>3}. {} ({}): {:.1} ms over {} waits, {share}",
            i + 1,
            it.lock,
            if it.value_type.is_empty() { "type not said" } else { it.value_type.as_str() },
            ms(it.wait_ns),
            it.waits
        );
        for site in &it.sites {
            _ = writeln!(
                out,
                "       at {}: {:.1} ms over {} waits",
                if site.site.is_empty() { "a site not said" } else { site.site.as_str() },
                ms(site.wait_ns),
                site.waits
            );
        }
        _ = writeln!(
            out,
            "       waited in {}; held in {}",
            it.top_waiter.as_deref().unwrap_or("no span"),
            it.top_holder.as_deref().unwrap_or("no span the holder said")
        );
        _ = writeln!(out, "       instead: {}", it.instead);
    }
    if report.locks_waited > report.locks.len() as u64 {
        _ = writeln!(
            out,
            "     ... and {} more (--top)",
            report.locks_waited - report.locks.len() as u64
        );
    }
    match report.stalls.is_empty() {
        true => _ = writeln!(out, "stalls: none"),
        false => {
            _ = writeln!(out, "stalls: {}", report.stalls.len());
            for it in &report.stalls {
                _ = writeln!(
                    out,
                    "  {} at {} ns: {} (threads {}; oldest {:.1} ms)",
                    it.kind,
                    it.t_ns,
                    it.line,
                    if it.threads.is_empty() { "not said" } else { it.threads.as_str() },
                    ms(it.oldest_ns)
                );
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(value_type: &str, mode: &str, thread: u64, holder: (&str, u64)) -> Row {
        Row {
            lock: "store.packs".into(),
            value_type: value_type.into(),
            mode: mode.into(),
            site: "src/a.rs:1".into(),
            thread,
            wait_ns: 10,
            holder_mode: holder.0.into(),
            holder_thread: holder.1,
            ..Row::default()
        }
    }

    fn said(rows: &[Row]) -> &'static str {
        let rows: Vec<&Row> = rows.iter().collect();
        recommend(&rows[0].value_type, &rows)
    }

    #[test]
    fn each_recommendation_is_read_from_the_rows_by_the_first_rule_that_holds() {
        // Reads behind writes, on a map: arc-swap wins, the first rule.
        let reads: Vec<Row> = (0..5)
            .map(|i| row("HashMap<u64, u64>", if i < 4 { "read" } else { "write" }, i, ("write", 9)))
            .collect();
        assert_eq!(said(&reads), ARC_SWAP);
        // The same reads behind readers: not arc-swap; a map.
        let behind_reads: Vec<Row> =
            (0..5).map(|i| row("blaze_hash::HashMap<K, V>", "read", i, ("read", 9))).collect();
        assert_eq!(said(&behind_reads), MAP);
        assert_eq!(said(&[row("u64", "lock", 1, ("lock", 2))]), ATOMIC);
        assert_eq!(said(&[row("bool", "lock", 1, ("", 0))]), ATOMIC);
        let queue = [
            row("alloc::collections::VecDeque<Job>", "lock", 1, ("lock", 3)),
            row("alloc::collections::VecDeque<Job>", "lock", 2, ("lock", 4)),
        ];
        assert_eq!(said(&queue), CHANNEL);
        // One waiter thread: no channel; one holder: its owner.
        let one_waiter = [
            row("alloc::vec::Vec<Job>", "lock", 1, ("lock", 3)),
            row("alloc::vec::Vec<Job>", "lock", 1, ("lock", 3)),
        ];
        assert_eq!(said(&one_waiter), OWNER);
        let two_holders = [
            row("blaze::State", "lock", 1, ("lock", 3)),
            row("blaze::State", "lock", 2, ("lock", 4)),
        ];
        assert_eq!(said(&two_holders), SHORTER);
        assert_eq!(said(&[row("blaze::State", "lock", 1, ("", 0))]), SHORTER, "no holder said");
    }

    #[test]
    fn locks_rank_by_total_wait_with_their_sites_and_spans() {
        let mut a = row("u64", "lock", 1, ("lock", 2));
        a.lock = "a".into();
        a.wait_ns = 30;
        a.waiter_span = "unit bodies".into();
        a.holder_span = "macro expand".into();
        let mut b = a.clone();
        b.lock = "b".into();
        b.wait_ns = 50;
        b.site = "src/b.rs:2".into();
        let mut c = b.clone();
        c.site = "src/b.rs:3".into();
        c.wait_ns = 5;
        let ranked = rank(&[a, c, b], 1000);
        assert_eq!(ranked.iter().map(|it| it.lock.as_str()).collect::<Vec<_>>(), ["b", "a"]);
        assert_eq!(ranked[0].wait_ns, 55);
        assert_eq!(ranked[0].worker_pct, Some(5.5));
        assert_eq!(ranked[0].sites[0].site, "src/b.rs:2", "the most waited site first");
        assert_eq!(ranked[1].top_waiter.as_deref(), Some("unit bodies"));
        assert_eq!(ranked[1].top_holder.as_deref(), Some("macro expand"));
    }

    #[test]
    fn a_lock_wait_event_is_kept_by_the_recorder() {
        assert!(crate::logs::keeps_event(TARGET, "event src/locks.rs:1", tracing::Level::INFO));
        assert!(crate::logs::keeps_event(STALL_TARGET, "event src/x.rs:1", tracing::Level::INFO));
    }
}
