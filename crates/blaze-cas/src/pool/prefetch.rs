//! What the pool prefetches, planned from metadata already in memory and never from a record
//! (docs/v2/plan/design/store-pool.md, section 5). This module is the first source, a view's
//! **trace**: what its last build had to read, in the order it needed it.
//!
//! - [`Recorder`]: a build's misses, appended on the miss path only (a reader that claims a
//!   stretch for a load says so). A hit records nothing and writes nothing.
//! - [`Trace`]: what is kept from one build of a view to the next ([`Trace::after`]): this
//!   build's misses in order, then the runs of the trace before it that are still loaded and were
//!   read. It names a page by its file and its page in the file, never by its window, so it
//!   means the same after a restart.
//! - [`plan`]: the traces of the views a build is for, as the runs to prefetch: in order, the
//!   pages already in memory or on their way dropped, each page once across views. The loads
//!   core replays them at the pace they are read ([`super::Loads::prefetch`],
//!   [`super::Loads::pump`]).
//!
//! Keeping a trace as a store record keyed by its view is the store's (it reads through the
//! pool from lane 4b); so are the other sources (the query graph's cone, lane 8).

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::collections::BTreeSet;

use super::{FileId, Give, Pool, Run, State, Windows};

/// Runs a recorder and a trace hold at most: past it a build's later misses are not recorded
/// (counted), and a trace's oldest runs are let go.
pub(crate) const TRACE_RUNS: usize = 1 << 16;

/// What a plan may walk: a run costs one and each of its pages one, so a plan of every trace of
/// a large build ends in a few milliseconds, saying how far it got.
pub(crate) const PLAN_FUEL: u64 = 1 << 21;

/// A build's misses, in order: each stretch of pages a reader claimed for a load.
#[derive(Default)]
pub(crate) struct Recorder {
    runs: Vec<Run>,
    dropped: u64,
}

impl Recorder {
    pub(crate) fn new() -> Recorder {
        Recorder::default()
    }

    /// A stretch a reader claimed for a load (the pool's pages), as it asks the load.
    pub(crate) fn missed(&mut self, run: Run) {
        match self.runs.len() < TRACE_RUNS {
            true => self.runs.push(run), // alloc: a build's misses, at most `TRACE_RUNS`
            false => self.dropped += 1,
        }
    }

    pub(crate) fn runs(&self) -> &[Run] {
        &self.runs
    }

    /// Misses not recorded: the recorder was full.
    pub(crate) fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Emptied for the next build, its room kept.
    pub(crate) fn clear(&mut self) {
        self.runs.clear();
        self.dropped = 0;
    }
}

/// A run of one file's pages, by the file: what a trace keeps.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Entry {
    pub(crate) id: FileId,
    /// Its first page, counted from the file's first.
    pub(crate) first: u32,
    pub(crate) pages: u32,
}

/// What a view's builds read, in the order to prefetch it for the next: at most [`TRACE_RUNS`]
/// entries, each page once.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub(crate) struct Trace {
    entries: Vec<Entry>,
}

/// The pages `first..first + pages` of `id` that `keep` says to keep and `seen` has not, added to
/// `entries` in order, a page that follows the last entry's lengthening it.
fn add(
    entries: &mut Vec<Entry>,
    seen: &mut BTreeSet<(FileId, u32)>,
    id: FileId,
    first: u32,
    pages: u32,
    keep: impl Fn(u32) -> bool,
) {
    for page in first..first.saturating_add(pages) {
        if !keep(page) || !seen.insert((id, page)) {
            continue;
        }
        match entries.last_mut() {
            Some(last) if last.id == id && last.first + last.pages == page => last.pages += 1,
            _ if entries.len() < TRACE_RUNS => entries.push(Entry { id, first: page, pages: 1 }), // alloc: the trace, capped
            _ => return,
        }
    }
}

impl Trace {
    pub(crate) fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The trace of a view after a build of it, from the one `before` and what the build
    /// `missed`:
    ///
    /// 1. the build's misses, in order: what the replay did not bring, or brought too late,
    ///    comes first next time;
    /// 2. then the pages of `before` that are `Loaded` and not left marked as prefetched: they
    ///    were read, by this build or since they were loaded. A page that was evicted and not
    ///    read again has gone cold and drops out, and so does one that was prefetched and never
    ///    read: a wrong guess is made once.
    ///
    /// A run of a file the pool no longer holds drops out too.
    pub(crate) fn after<G: Give>(before: &Trace, missed: &Recorder, pool: &Pool<G>, windows: &Windows) -> Trace {
        let mut entries = Vec::new(); // alloc: the trace, capped at `TRACE_RUNS`
        let mut seen = BTreeSet::new(); // alloc: a build's end, each page of the trace once
        for &run in missed.runs() {
            let Some(file) = windows.file_at(run.first) else { continue };
            let first = (run.first - file.first_page()) as u32;
            add(&mut entries, &mut seen, file.id, first, run.pages, |_| true);
        }
        for entry in &before.entries {
            let Some(file) = windows.get(entry.id) else { continue };
            let (base, committed) = (file.first_page(), file.pages());
            add(&mut entries, &mut seen, entry.id, entry.first, entry.pages, |page| {
                if u64::from(page) >= committed {
                    return false;
                }
                let word = pool.word_now(base + u64::from(page));
                word.state() == State::Loaded && !word.prefetched()
            });
        }
        Trace { entries }
    }
}

/// How far a plan got (rule `bounded-progress`): the entries it walked of those its traces hold,
/// and the pages it asked for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Planned {
    pub(crate) walked: usize,
    pub(crate) of: usize,
    pub(crate) pages: u64,
}

/// The runs to prefetch for a build of the views whose `traces` these are, in the order to read
/// them: each trace's entries in its order, a page that is not `Unloaded` (loaded, on its way,
/// or refused) dropped, a page an earlier trace named dropped, and runs that follow each other
/// merged. For [`super::Loads::prefetch`].
///
/// It walks until `fuel` is out ([`PLAN_FUEL`]): a run costs one, each page one.
pub(crate) fn plan<G: Give>(traces: &[&Trace], pool: &Pool<G>, windows: &Windows, fuel: u64) -> (Vec<Run>, Planned) {
    let of: usize = traces.iter().map(|it| it.entries.len()).sum();
    let mut runs: Vec<Run> = Vec::new(); // alloc: the plan, once a build
    let mut seen = BTreeSet::new(); // alloc: the plan's pages, once a build
    let (mut walked, mut pages, mut fuel) = (0usize, 0u64, fuel);
    'traces: for trace in traces {
        for entry in &trace.entries {
            let cost = 1 + u64::from(entry.pages);
            if fuel < cost {
                break 'traces;
            }
            fuel -= cost;
            walked += 1;
            let Some(file) = windows.get(entry.id) else { continue };
            for page in entry.first..entry.first.saturating_add(entry.pages) {
                if u64::from(page) >= file.pages() {
                    break;
                }
                let at = file.first_page() + u64::from(page);
                if pool.word_now(at).state() != State::Unloaded || !seen.insert(at) {
                    continue;
                }
                match runs.last_mut() {
                    Some(last) if last.first + u64::from(last.pages) == at => last.pages += 1,
                    _ => runs.push(Run { first: at, pages: 1 }),
                }
                pages += 1;
            }
        }
    }
    (runs, Planned { walked, of, pages })
}
