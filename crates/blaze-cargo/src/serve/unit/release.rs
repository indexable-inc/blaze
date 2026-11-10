//! What a build lets go of its front end once nothing still to compile reads it (B5, #806,
//! #734): a crate's bodies' inference and trait solver caches once every unit of it compiled,
//! and the MIR, constant values and solver caches of every crate once no unit is between its
//! phases or memory is short. Without it a
//! build's MIR, inference and constants stay resident to its end (coord-102: about 6 GiB of MIR
//! and 3 GiB of inference of 21 small crates).
//!
//! A drop needs the front end's database exclusive (a value is read in place, `blaze_query`'s
//! row rule 3), so it is a write of the job's [`super::UnitWriter`] as a build script's run is,
//! applied by `Writer::apply` once no reader is alive:
//!
//! - **With another write.** A wave of build scripts' runs or proc macros' loads already stops
//!   the readers; the crates finished by then are let go with it, at no stop of their own.
//! - **When no unit is open.** Every unit that began has compiled, so no reader is left to
//!   wait for, and every MIR held is of a finished crate or of none.
//! - **When memory is short** ([`Squeeze`]): blaze past its limit's line, or a subsystem a
//!   release lowers past its share. The build stops its readers for it (each task under way
//!   ends first; none is cancelled), and the front end's values kept by bytes are swept under
//!   the squeeze's caps too ([`blaze_query::db::Storage::sweep_values`]), not only at the next
//!   view.
//! - **Once the front end is kept**, its build over: every MIR goes then, of every goal and
//!   under any limit or none ([`Release::end`]). MIR is the request's: a value of the build
//!   that made it, read again by the next build only as its memo (verified against, made
//!   again where read), so nothing of it outlives the build that made it. This release is
//!   never on the loop and never before the build's answer: it is kept with the front end
//!   and run by the task that stores the front end, which holds it alone as the writer did
//!   (`Steps::release_kept`). It only lowers memory, so the checkout's next build never waits
//!   for it: a request that waits for the front end has it handed back unreleased, and that
//!   build's own releases, at the latest its end's, let the MIR go. When blaze is past its
//!   limit's line the release runs all the same, before that build is given the front end:
//!   memory short stays a reason that stops for nothing but the answer.
//!
//! A value let go keeps its memo, so a read of it later in the build, or by the next build of
//! the head, makes it again and backdates it: nothing read can differ, a re-read costs time and
//! is named by its query's reruns.
//!
//! A check (`Goal::Check`) keeps its inference: what it made, and what the next check of the
//! head reads again. Its MIR goes as its front end is kept, as every build's does, and memory
//! makes a release of a check due mid-build as of any build: then the MIR, constant values and
//! solver caches of every crate go and the values kept by bytes are swept under the squeeze's
//! caps, but no crate's inference is let go by name. A check that let go of nothing (the
//! release before slowcheck, and before this, a check under no limit) kept every MIR it made in
//! the kept front end, for the next check and the one after (MIR queries are `lru = 0`: let go
//! only when asked), so each check grew blaze's heap by what it made (build 19 of coord
//! nfcheck: 127 GiB live against a budget of 59, 110 GiB of it MIR), and the plane, which grants
//! a task memory only when what the process holds leaves room, then ran every unit's phase
//! alone: one worker of 16 busy for the whole 43-minute check.

#![forbid(unsafe_code)]

use std::{alloc::Global, hash::Hash, sync::atomic::{AtomicU32, Ordering}};

/// The front end releases run since the daemon started, every build's: the build view's
/// `releases` ([`crate::serve::job::doing::memory`]), a counter that only a release moves.
pub(crate) static RELEASES: AtomicU32 = AtomicU32::new(0);

use base_db::Crate;
use blaze_graph::admission::{self, Pressure, Squeeze};
use blaze_hash::HashMap;
use blaze_lower::{Crates, Lowered};
use blaze_query::db::{Database as _, MemoryUsage};

#[cfg(test)]
mod tests;

/// Where each unit of a build stands, for [`Finished`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stand {
    NotBegun,
    /// Its first task started and its compile has not ended.
    Open,
    Compiled,
}

/// Which crates of a build every unit of has compiled, and whether a unit is open. Keyed by
/// `K`, a crate (`base_db::Crate` in the daemon), so it is told without a database.
pub(crate) struct Finished<K> {
    /// Each unit's crate; none for a build script's run.
    of: Vec<Option<K>>,
    stands: Vec<Stand>,
    /// Each crate's units whose compile has not ended.
    left: HashMap<K, u32, Global>, // alloc: one entry a crate, once per build
    /// Crates whose every unit compiled, in the order the last one did, not taken yet.
    ready: Vec<K>,
    /// Units [`Stand::Open`]. A unit that panicked stays open (no compile ends it), and the
    /// build's MIR then goes only when memory is short or the front end is kept.
    open: u32,
}

impl<K: Copy + Eq + Hash> Finished<K> {
    /// A build whose unit `unit` is of crate `of[unit]`.
    pub(crate) fn new(of: Vec<Option<K>>) -> Finished<K> {
        let mut left = HashMap::new_in(Global); // alloc: as the field
        for krate in of.iter().flatten() {
            *left.entry(*krate).or_insert(0) += 1;
        }
        let stands = vec![Stand::NotBegun; of.len()]; // alloc: a stand a unit, once per build
        Finished { of, stands, left, ready: Vec::new(), open: 0 }
    }

    /// Unit `unit`'s first task started. A build script's run has no crate and no compile to
    /// end it, so it never opens.
    pub(crate) fn began(&mut self, unit: usize) {
        if self.of.get(unit).copied().flatten().is_none() {
            return;
        }
        let Some(stand) = self.stands.get_mut(unit) else { return };
        if *stand == Stand::NotBegun {
            *stand = Stand::Open;
            self.open += 1;
        }
    }

    /// Unit `unit`'s compile ended, ok or not; a unit told twice counts once.
    pub(crate) fn compiled(&mut self, unit: usize) {
        let Some(stand) = self.stands.get_mut(unit) else { return };
        match *stand {
            Stand::Compiled => return,
            Stand::Open => self.open -= 1,
            Stand::NotBegun => {}
        }
        *stand = Stand::Compiled;
        let Some(krate) = self.of[unit] else { return };
        if let Some(left) = self.left.get_mut(&krate) {
            *left -= 1;
            if *left == 0 {
                self.ready.push(krate); // alloc: a crate a build, at most
            }
        }
    }

    /// Whether no unit is open: every unit that began has compiled.
    pub(crate) fn idle(&self) -> bool {
        self.open == 0
    }

    pub(crate) fn has_ready(&self) -> bool {
        !self.ready.is_empty()
    }

    /// The crates finished since the last take.
    pub(crate) fn take(&mut self) -> Vec<K> {
        std::mem::take(&mut self.ready)
    }
}

/// A job's releases of its front end: what is finished, and whether memory makes one due.
/// Keyed by `K` as [`Finished`] is, a crate in the daemon.
pub(crate) struct Release<K = Crate> {
    finished: Finished<K>,
    squeeze: Squeeze,
    /// Memory made a release due ([`Squeeze::due`], asked as each unit compiles).
    pressed: bool,
    /// Whether the build compiles code. A check's front end lets go mid-build only when memory
    /// makes a release due, and then of no crate's inference by name.
    compiles: bool,
    /// The build is over and its front end is to be kept ([`Release::end`]): the next release
    /// lets go of every MIR, whatever the goal and memory.
    ended: bool,
}

/// What one release lets go of, told by [`Release::plan`].
#[derive(PartialEq, Eq, Debug)]
pub(crate) struct Plan<K> {
    /// The crates finished since the last release: their bodies' inference goes. Always
    /// empty for a check, whose inference the next check of the head reads again.
    pub(crate) crates: Vec<K>,
    /// Every MIR and constant's value goes too: no unit is open, or memory is short.
    pub(crate) mir: bool,
    /// Memory made it due: the values kept by bytes are swept under the squeeze's caps.
    pub(crate) pressed: bool,
    /// The build is over ([`Release::end`]): its MIR goes with it.
    pub(crate) ended: bool,
}

impl Release<Crate> {
    /// For a build of `units` units, of the crates `crates` names; `compiles` unless it is a
    /// check.
    pub(crate) fn new(crates: &Crates, units: usize, compiles: bool) -> Release {
        let of = (0..units).map(|unit| crates.krate(unit)).collect(); // alloc: a crate a unit, once per build
        Release::of(Finished::new(of), compiles)
    }

    /// Unit `unit` compiled: whether memory makes a release due is asked now, once a unit,
    /// with one snapshot of the tags.
    pub(crate) fn compiled(&mut self, unit: usize) {
        self.compiled_under(unit, Pressure::now);
    }

    /// With the front end exclusive (no reader alive): lets go of what the finished crates
    /// keep, and of every MIR when no unit is open or memory made this release due, then under
    /// memory pressure sweeps the values kept by bytes under the squeeze's caps. A span names
    /// what it let go and what blaze held before and after (rule 16).
    pub(crate) fn release(&mut self, lowered: &mut Lowered) {
        let Some(Plan { crates, mir, pressed, ended }) = self.plan() else { return };
        RELEASES.fetch_add(1, Ordering::Relaxed);
        let before = Pressure::now();
        let span = tracing::info_span!(
            target: "blaze",
            "front end release",
            pressed,
            ended,
            crates = crates.len(),
            mir,
            held = before.held,
            limit = before.limit,
            over = before.over.map_or("", |it| it.name()),
            bodies = tracing::field::Empty,
            inference = tracing::field::Empty,
            mir_dropped = tracing::field::Empty,
            const_eval = tracing::field::Empty,
            solver_caches = tracing::field::Empty,
            intern_tables = tracing::field::Empty,
            unread = tracing::field::Empty,
            left = tracing::field::Empty,
            query_storage = tracing::field::Empty,
        );
        let _release = span.enter();
        let db = lowered.served_db();
        let dropped = hir::evict::drop_crates_after_store(db, &crates, mir);
        if pressed {
            db.storage_mut().sweep_values(&Squeeze::caps(before));
        }
        // The build is over: what neither it nor the build before it read of the front end's
        // evictable values goes, the results of revisions no edit since has asked for. Only
        // here, with the build's reads all made; a build that wrote nothing lets nothing go.
        let unread = match ended {
            true => db.storage_mut().end_epoch(),
            false => blaze_query::db::Unread::default(),
        };
        // What was let go goes back to the system now, not when mimalloc's purge delay and the
        // next page taken get to it: this thread's heaps and every freed page at once, each
        // other thread's heaps as it next allocates or idles (memcap5).
        blaze_mem::tag::give_back_heap();
        let after = Pressure::now();
        if pressed {
            self.squeeze.released(after);
        }
        span.record("bodies", dropped.bodies);
        span.record("inference", dropped.inference);
        span.record("mir_dropped", dropped.mir);
        span.record("const_eval", dropped.const_eval);
        span.record("solver_caches", dropped.solver_caches);
        span.record("intern_tables", dropped.intern_tables);
        span.record("unread", unread.values);
        span.record("left", after.held);
        // What the query storage still holds after the drops. On a release memory made due, by
        // ingredient (memcap6): its rows in use are a walk of every function's rows made, a
        // read of every row page, which the build's readers wait out (the front end is
        // exclusive), so it runs only when that report is what the squeeze wants. Otherwise
        // the total alone, from counters.
        let storage = if pressed {
            let mut usage = Vec::new(); // alloc: the release's report, an entry an ingredient
            db.storage().memory_usage(&mut usage);
            Holders::of(usage)
        } else {
            Holders::total(db.storage().held_bytes())
        };
        span.record("query_storage", storage.total);
        // One line each release in the daemon's log, on its standard error as the memory line
        // is: why, what went, blaze's memory before and after against the limit (B5), and the
        // query storage's largest holders.
        eprintln!(
            "blaze serve: {}",
            Said {
                crates: crates.len(),
                mir,
                pressed,
                ended,
                dropped,
                unread,
                before,
                after,
                storage,
            }
        );
    }
}

/// The line a release says ([`Release::release`]): why it ran, what it let go and how much
/// blaze held before and after it, against the limit, then what the query storage holds.
pub(crate) struct Said {
    pub(crate) crates: usize,
    pub(crate) mir: bool,
    pub(crate) pressed: bool,
    pub(crate) ended: bool,
    pub(crate) dropped: hir::evict::Dropped,
    /// What the build's end let go of that two builds left unread
    /// ([`blaze_query::db::Storage::end_epoch`]).
    pub(crate) unread: blaze_query::db::Unread,
    pub(crate) before: Pressure,
    pub(crate) after: Pressure,
    pub(crate) storage: Holders,
}

/// What the front end's query storage holds, said at the end of a release's line. On a release
/// memory made due, as [`blaze_query::db::Storage::memory_usage`] reports it, with its largest
/// holders: each ingredient and dependency graph column ranked by what it holds (its rows and
/// what its values hold beside them), with its rows made and, where fewer, its rows in use, so
/// a function's pages made for keys it never ran read apart (one page of 1024 rows per ID page
/// it has a key in). On any other, its total alone ([`blaze_query::db::Storage::held_bytes`]).
pub(crate) struct Holders {
    /// Every entry, most held first; none when only the total was read.
    usage: Vec<MemoryUsage>,
    /// What all of them hold.
    pub(crate) total: u64,
}

impl Holders {
    /// The holders said at most.
    const SHOWN: usize = 8;

    pub(crate) fn of(mut usage: Vec<MemoryUsage>) -> Holders {
        usage.sort_by(|a, b| b.held().cmp(&a.held()).then(a.name.cmp(b.name)));
        let total = usage.iter().map(MemoryUsage::held).sum();
        Holders { usage, total }
    }

    /// The total alone, with no holder said.
    pub(crate) fn total(total: u64) -> Holders {
        Holders { usage: Vec::new(), total }
    }
}

/// A count of rows as a holder says it: `950`, `12.3k`, `3.19M`. The unit is picked after
/// rounding, so a count that rounds up to the next unit says it there (999,950 is `1.00M`, not
/// `1000.0k`).
pub(crate) struct Rows(pub(crate) u64);

impl std::fmt::Display for Rows {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let rows = self.0;
        if rows < 1_000 {
            return write!(f, "{rows}");
        }
        // Tenths of a thousand, rounded half up.
        let tenths = rows.saturating_add(50) / 100;
        if tenths < 10_000 {
            return write!(f, "{}.{}k", tenths / 10, tenths % 10);
        }
        // Hundredths of a million, rounded half up.
        let hundredths = rows.saturating_add(5_000) / 10_000;
        write!(f, "{}.{:02}M", hundredths / 100, hundredths % 100)
    }
}

impl std::fmt::Display for Holders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let gib = |bytes: u64| bytes as f64 / f64::from(1u32 << 30);
        write!(f, "the query storage holds {:.2} GiB", gib(self.total))?;
        for (at, usage) in self.usage.iter().take(Self::SHOWN).enumerate() {
            f.write_str(if at == 0 { ": " } else { ", " })?;
            write!(f, "{} {:.2} ({} rows", usage.name, gib(usage.held()), Rows(usage.rows))?;
            if usage.used < usage.rows {
                write!(f, " made, {} used", Rows(usage.used))?;
            }
            f.write_str(")")?;
        }
        Ok(())
    }
}

impl std::fmt::Display for Said {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let gib = |bytes: u64| bytes as f64 / f64::from(1u32 << 30);
        let Said { crates, mir, pressed, ended, dropped, unread, before, after, storage: _ } =
            self;
        f.write_str("front end release (")?;
        match (*pressed, before.over) {
            (true, _) if before.squeezed() => write!(
                f,
                "memory: blaze is past the limit's line of {:.2} GiB",
                gib(before.limit - before.limit / admission::LINE)
            )?,
            (true, Some(tag)) => write!(
                f,
                "memory: {} is past its share of {:.2} GiB",
                tag.name(),
                gib(admission::fair_share(tag, before.limit))
            )?,
            (true, None) => f.write_str("memory")?,
            (false, _) if *ended => f.write_str("the build is over")?,
            (false, _) if *mir => f.write_str("no unit is open")?,
            (false, _) => f.write_str("crates finished")?,
        }
        write!(
            f,
            "): let go of {} inference results of {} bodies in {crates} crates",
            dropped.inference, dropped.bodies
        )?;
        if *mir {
            write!(f, ", {} MIR bodies, {} constant values", dropped.mir, dropped.const_eval)?;
        }
        write!(f, " and {} trait solver caches", dropped.solver_caches)?;
        if *pressed {
            f.write_str(", and swept the front end's values to their fair shares")?;
        }
        if unread.values != 0 {
            write!(f, ", and {} values the last two builds did not read", unread.values)?;
        }
        write!(f, "; blaze held {:.2} GiB, now {:.2} GiB", gib(before.held), gib(after.held))?;
        match before.held.checked_sub(after.held) {
            Some(freed) => write!(f, " ({:.2} freed)", gib(freed))?,
            None => write!(
                f,
                " ({:.2} more: other tasks grew meanwhile)",
                gib(after.held - before.held)
            )?,
        }
        match before.limit {
            u64::MAX => f.write_str(", under no limit")?,
            limit => write!(f, ", of a limit of {:.2} GiB", gib(limit))?,
        }
        write!(f, "; {}", self.storage)
    }
}

impl<K: Copy + Eq + Hash> Release<K> {
    pub(crate) fn of(finished: Finished<K>, compiles: bool) -> Release<K> {
        Release { finished, squeeze: Squeeze::default(), pressed: false, compiles, ended: false }
    }

    pub(crate) fn began(&mut self, unit: usize) {
        self.finished.began(unit);
    }

    /// Unit `unit` compiled, memory read by `now` (only if it can matter): a check's as any
    /// build's, since memory past the limit stays past it until something is let go.
    pub(crate) fn compiled_under(&mut self, unit: usize, now: impl FnOnce() -> Pressure) {
        self.finished.compiled(unit);
        if !self.pressed {
            self.pressed = self.squeeze.due(now());
        }
    }

    /// The plane holds a task back for memory the process holds besides its tasks
    /// (`blaze_graph::Plane::short`): a release is due if [`Squeeze::short`] says so, read
    /// by `now` only if it can matter. Its sweep caps the values the front end keeps by bytes
    /// at their fair shares and drops every MIR, as one memory made due does.
    pub(crate) fn short_under(&mut self, now: impl FnOnce() -> Pressure) {
        if !self.pressed {
            self.pressed = self.squeeze.short(now());
        }
    }

    /// The system says memory is short (docs/memory-budget.md section 7, "Pressure"): a
    /// release is due now, past [`Squeeze`]'s step, which counts only this daemon's growth. The
    /// daemon asks at most once per [`blaze_graph::pressure::HOLD_NS`] of a run of events
    /// ([`blaze_graph::pressure::Pressured::pressed`]), so a long run does not sweep the front
    /// end over and over.
    pub(crate) fn pressed(&mut self) {
        self.pressed = true;
    }

    /// The build is over and its front end is kept, no reader alive: the next release lets go
    /// of every MIR, of a check as of any build, under any limit or none. What a build made of
    /// MIR is the build's: kept, it is only what the next build would verify its memos
    /// against, which needs the memo alone. That release is run after the build's answer, by
    /// the task that stores the kept front end (the module's list).
    pub(crate) fn end(&mut self) {
        self.ended = true;
    }

    /// Whether the build should stop its readers for a release: memory makes one due, the
    /// build is over, or the build compiles and no unit is open and a crate is finished (no
    /// reader to wait for).
    pub(crate) fn due(&self) -> bool {
        self.pressed
            || self.ended
            || (self.compiles && self.finished.idle() && self.finished.has_ready())
    }

    /// What a release now lets go of, taking it (a second plan is of what finished since);
    /// none when there is nothing. A check's is due to memory or to its end alone and names no
    /// crate: its MIR, constant values and solver caches go (and under memory, its values are
    /// swept), its inference kept for the next check of the head except as the sweep's caps
    /// take it. Every build's last, at its end, lets go of every MIR.
    pub(crate) fn plan(&mut self) -> Option<Plan<K>> {
        let pressed = std::mem::take(&mut self.pressed);
        let ended = std::mem::take(&mut self.ended);
        if !self.compiles {
            return (pressed || ended).then(|| Plan {
                crates: Vec::new(),
                mir: true,
                pressed,
                ended,
            });
        }
        let crates = self.finished.take();
        let mir = pressed || ended || self.finished.idle();
        if crates.is_empty() && !pressed && !ended {
            return None;
        }
        Some(Plan { crates, mir, pressed, ended })
    }
}
