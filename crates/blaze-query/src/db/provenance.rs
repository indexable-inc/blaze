//! Provenance: what each memo brought up to a view did in it, and why it ran. A run records
//! one word on its node: the view and its cause, the first of its reads the claimed verify
//! found changed (an input field or a memo, by node), or why it ran with none. A hit and a
//! verify record nothing; [`Storage::provenance`] reads a view's table back from the nodes'
//! colors and these words.
//!
//! A cause is a node of the view's table when the memo was current in the view before (a
//! dependency that changed since is one that ran in this view); a memo verified in an earlier
//! view may name a dependency that ran in a view between, whose own word is that view's.
//!
//! The run log ([`log_runs`], [`take_log_in`]) is the same causes as rows a reader outside the
//! storage can keep: while a host asks for it (the daemon, while it keeps a profile history),
//! each run but a first one ([`Cause::New`], counted per ingredient in `counts::RAN_NEW` alone)
//! is logged with its cause and, for [`Cause::Changed`], the read that changed; and the first
//! untracked read of each run ([`super::report_untracked_read`]) is logged with its
//! [`super::Untracked::what`], so a row that reruns every view names the read that makes it.
//! A first run is not logged: a cold build's every run is one. Each storage keeps its own
//! ([`RunLog`]), as node keys alone, bounded ([`LOG_MOST`]: past it a run is counted, not
//! kept); its holder takes it with the database in hand, which names each memo by its key as
//! the key's type shows it and each input by its [`name_rows`] namer: text is made only for
//! what is taken, never as a run is logged.

use std::{
    fmt::Write as _,
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use crossbeam_queue::SegQueue;

use super::{
    DatabaseKeyIndex, Id, Read, Storage,
    capture::read_of_key,
    ingredient::{self, Kind},
    row::VIEW_MASK,
    storage::{Restamp, key_parts},
};

/// Why a memo ran.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cause {
    /// The first of its reads its verify found changed since it last ran: an input or
    /// tracked field or a memo, by node ([`Storage::node_read`]).
    Changed(u32),
    /// Its first run.
    New,
    /// Its value was evicted: proved unchanged, then run for a reader that wants the value.
    Evicted,
    /// It made an untracked read, so it runs in every view.
    Untracked,
    /// A cycle's: a stale provisional row, or a verify that re-entered a cycle.
    Cycle,
}

/// What a memo did in a view, besides a plain hit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// Proved unchanged without running.
    Verified,
    /// Ran, and its value changed: its readers verify against it.
    Reran,
    /// Ran to a value equal to its old one: its readers stop here (early cutoff).
    ReranSameOutput,
}

/// One memo of a view's table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Provenance {
    /// Its node: what a [`Cause::Changed`] names.
    pub node: u32,
    pub key: DatabaseKeyIndex,
    /// Over the whole view: a memo first made in it is [`Outcome::Reran`] however it ran after.
    pub outcome: Outcome,
    /// Why it last ran in the view; `None` when verified.
    pub cause: Option<Cause>,
}

const REASON_SHIFT: u32 = 28;

impl Cause {
    /// The word a run in `view` records.
    pub(crate) fn word(self, view: u32) -> u64 {
        let (reason, node) = match self {
            Cause::Changed(node) => (0, node),
            Cause::New => (1, 0),
            Cause::Evicted => (2, 0),
            Cause::Untracked => (3, 0),
            Cause::Cycle => (4, 0),
        };
        (u64::from(view | reason << REASON_SHIFT) << 32) | u64::from(node)
    }

    /// The view and cause of a run's word; view 0 when the node never ran.
    fn of(word: u64) -> (u32, Cause) {
        let high = (word >> 32) as u32;
        let cause = match high >> REASON_SHIFT {
            0 => Cause::Changed(word as u32),
            1 => Cause::New,
            2 => Cause::Evicted,
            3 => Cause::Untracked,
            _ => Cause::Cycle,
        };
        (high & VIEW_MASK, cause)
    }
}

/// A run's word with its view passed through `restamp` (the renumbering).
pub(crate) fn restamp_run(word: u64, restamp: &mut Restamp<'_>) -> u64 {
    if word == 0 {
        return 0;
    }
    let (view, cause) = Cause::of(word);
    cause.word(restamp.apply(view))
}

impl Storage {
    /// The memos brought up to this view other than by a plain hit, pushed onto `out` in
    /// node order: each one's outcome and, if it ran, why. Read after the view's queries
    /// ended; a query still running is left out or shown as it was.
    pub fn provenance(&self, out: &mut Vec<Provenance>) {
        let view = self.view_bits();
        for node in 1..self.nodes_made() {
            let painted = self.color_of(node);
            if (painted >> 32) as u32 != view {
                continue;
            }
            let (index, id, _) = key_parts(self.key_of(node));
            if ingredient::kind(index) != Some(Kind::Function) {
                continue;
            }
            let (ran, cause) = Cause::of(self.runs.get(node as usize).load(Ordering::Relaxed));
            let outcome = match (ran == view, painted as u32 == view) {
                (false, _) => Outcome::Verified,
                (true, true) => Outcome::Reran,
                (true, false) => Outcome::ReranSameOutput,
            };
            // alloc: the caller's table, one entry per memo brought up to the view
            out.push(Provenance {
                node,
                key: DatabaseKeyIndex::new(index, id),
                outcome,
                cause: (ran == view).then_some(cause),
            });
        }
    }

    /// What node `node` is: a memo, or an input's or tracked struct's field.
    ///
    /// # Panics
    ///
    /// If no such node was made.
    pub fn node_read(&self, node: u32) -> Read {
        assert!(
            node != 0 && node < self.nodes_made(),
            "no node {node}: {} were made",
            self.nodes_made()
        );
        read_of_key(self.key_of(node))
    }
}

/// The most entries a storage's run log holds between two takes: 32 bytes each, so 16 MiB.
/// Past it a run is counted in [`Taken::dropped`], never kept over an older one.
pub const LOG_MOST: u64 = 1 << 19;

static LOGGING: AtomicBool = AtomicBool::new(false);

/// A storage's run log: what its runs logged since its holder last took it
/// ([`take_log_in`]), as node keys, so logging formats nothing and allocates only the queue's
/// blocks.
pub(crate) struct RunLog {
    entries: SegQueue<Entry>,
    logged: AtomicU64,
    dropped: AtomicU64,
}

impl RunLog {
    pub(crate) const fn new() -> RunLog {
        RunLog { entries: SegQueue::new(), logged: AtomicU64::new(0), dropped: AtomicU64::new(0) }
    }

    /// Logs one event, past [`LOG_MOST`] counting it instead.
    #[cold]
    #[inline(never)]
    pub(crate) fn log(&self, key: u64, read: u64, view: u32, why: Logged) {
        if self.logged.fetch_add(1, Ordering::Relaxed) >= LOG_MOST {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.entries.push(Entry { key, read, view, why });
    }
}

/// One logged event, as the engine knows it: node keys ([`key_parts`]) and a static reason.
#[derive(Clone, Copy)]
struct Entry {
    /// The node key of the memo that ran (or made the untracked read).
    key: u64,
    /// For [`Cause::Changed`], the node key of the read that changed; else 0.
    read: u64,
    view: u32,
    why: Logged,
}

/// Why an entry of the run log was logged.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Logged {
    /// The memo ran, for this cause (never [`Cause::New`]).
    Ran(Cause),
    /// The memo's run made an untracked read: its first one in that run, named.
    UntrackedRead(&'static str),
}

/// One row of the run log, as [`take_log_in`] gives it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LoggedRun {
    /// The memo: its function's name.
    pub query: &'static str,
    /// The memo, as [`take_log_in`] names a node: `` `query`(<its key as its type shows it>) ``,
    /// which a [`LoggedRun::read`] of another row names it by.
    pub key: String,
    pub view: u32,
    pub why: Logged,
    /// For [`Cause::Changed`], the read that changed, named as `key` is: an input's or tracked
    /// struct's field (`` `Input.field`(<its row's name>) ``) or another memo.
    pub read: Option<String>,
    /// What that read is of: `input`, `tracked`, `interned` or `function`.
    pub read_kind: Option<&'static str>,
}

/// What [`take_log_in`] took: the rows, sorted, and the runs past [`LOG_MOST`] it could not
/// keep.
#[derive(Default, Debug)]
pub struct Taken {
    pub rows: Vec<LoggedRun>,
    pub dropped: u64,
}

/// Turns the run log on or off, for every storage. Off, logging costs a run one relaxed load.
pub fn log_runs(on: bool) {
    LOGGING.store(on, Ordering::Relaxed);
}

#[inline]
pub(crate) fn logging() -> bool {
    LOGGING.load(Ordering::Relaxed)
}

/// Names an input's or tracked struct's row for the run log: its ingredient's name, its ID and
/// where to write; true when it wrote a name (a file's path, a crate's name), false to leave it
/// to its ID. Called with the database the log is taken with attached
/// ([`super::with_attached_db`]).
pub type RowNamer = fn(ingredient: &'static str, id: Id, out: &mut String) -> bool;

static NAMER: OnceLock<RowNamer> = OnceLock::new();

/// Sets the process's [`RowNamer`]: the front end's, once; a second is ignored, as the first
/// names the same rows.
pub fn name_rows(namer: RowNamer) {
    _ = NAMER.set(namer);
}

/// Everything `db`'s storage logged since the last take, each node named with `db` attached
/// (a memo by its key as its type shows it, a field by its row's [`name_rows`] name, else by
/// ID) and sorted by memo, view, reason and read, so the same runs give the same rows in the
/// same order whatever threads ran them (rule 25). Called by the database's holder outside any
/// query (the daemon, as a build ends), so a key's `Debug` may read the database.
pub fn take_log_in<D: super::Database>(db: &D) -> Taken {
    let storage = db.storage();
    let log = &storage.run_log;
    super::attach(db, || {
        // alloc: once a take (a build's end), a name a node named
        let mut names: crate::table::FxMap<u64, String> = crate::table::FxMap::default();
        let mut name =
            |key: u64| names.entry(key).or_insert_with(|| node_name(storage, key)).clone(); // clone: a row's own text
        // alloc: once a take, a row an entry
        let mut rows = Vec::new();
        while let Some(entry) = log.entries.pop() {
            let (index, _, _) = key_parts(entry.key);
            let read = (entry.read != 0).then(|| name(entry.read));
            let read_kind = (entry.read != 0).then(|| {
                let (index, _, _) = key_parts(entry.read);
                match ingredient::kind(index) {
                    Some(Kind::Input) => "input",
                    Some(Kind::Tracked) => "tracked",
                    Some(Kind::Interned) => "interned",
                    Some(Kind::Function) | None => "function",
                }
            });
            rows.push(LoggedRun {
                query: ingredient::name(index),
                key: name(entry.key),
                view: entry.view,
                why: entry.why,
                read,
                read_kind,
            });
        }
        log.logged.store(0, Ordering::Relaxed);
        let dropped = log.dropped.swap(0, Ordering::Relaxed);
        rows.sort_unstable_by(|a, b| {
            (a.query, &a.key, a.view, why_order(a.why), &a.read).cmp(&(
                b.query,
                &b.key,
                b.view,
                why_order(b.why),
                &b.read,
            ))
        });
        Taken { rows, dropped }
    })
}

/// A total order of reasons, for [`take_log_in`]'s sort.
fn why_order(why: Logged) -> (u8, &'static str) {
    match why {
        Logged::Ran(Cause::Changed(_)) => (0, ""),
        Logged::Ran(Cause::New) => (1, ""),
        Logged::Ran(Cause::Evicted) => (2, ""),
        Logged::Ran(Cause::Untracked) => (3, ""),
        Logged::Ran(Cause::Cycle) => (4, ""),
        Logged::UntrackedRead(what) => (5, what),
    }
}

/// A node key as the run log names it, with its database attached: a memo as
/// `` `query`(<key>) ``, its key as its type shows it ([`super::key_text`], its ID where the
/// key has no `Debug` or it fails), and an input's or tracked struct's field as
/// `` `Input.field`(<row>) ``, its row as the [`name_rows`] namer names it, else its ID. The
/// same text names a memo where it ran and where another's run read it, so a chain of causes
/// is a join on it.
fn node_name(storage: &Storage, key: u64) -> String {
    let (index, id, field) = key_parts(key);
    let name = ingredient::name(index);
    let mut out = String::new(); // alloc: a log row's name, once a node a take
    match ingredient::vtable(index).map(|it| (it.kind, it.fields)) {
        Some((Kind::Input | Kind::Tracked, fields)) => {
            let field = fields.get(field as usize).copied().unwrap_or("?");
            _ = write!(out, "`{name}.{field}`(");
            let start = out.len();
            let named = NAMER.get().is_some_and(|namer| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| namer(name, id, &mut out)))
                    .unwrap_or(false)
            });
            if !named {
                out.truncate(start);
                _ = write!(out, "{id:?}");
            }
            out.push(')');
        }
        _ => _ = write!(out, "`{name}`({})", super::key_text(storage, index, id)),
    }
    out
}
