//! The live tasks (tables `tasks` and `task_edges`, docs/v2/plan/design/explore-generic.md),
//! folded through the one model of a program as it runs, `blaze_protocol::now::Fold`
//! (explore-core.md decision 4): the same core `blaze status` and a build's client fold the
//! plane with, so the page's Running now and the terminal's say the same thing.
//!
//! - A row is upserted by (process, task), whole. A new task, or one whose kind or name
//!   changed, is [`Fold::take`]n with the deps its edges name; a state that changed is
//!   [`Fold::moved`]. A task's edges are the latest set its `task_edges` rows said; a new set
//!   takes it again.
//! - Downloads (kind [`DOWNLOAD`], progress in bytes) are no tasks of the fold: each one in
//!   flight is a [`now::Download`] of the look, its rate from the last few samples of its bytes.
//! - The fold's ids are its own, one per (process, task) as first said, so two processes'
//!   tasks never meet; [`Tasks::key`] and [`Tasks::fold_id`] map between them.
//! - A settled task leaves [`SETTLED_NS`] after its `end_ns`, on the frames' `at_ns`, as the
//!   source evicts it. The fold keeps every task it was given, so once more tasks left than
//!   are live, it is made anew from the live ones ([`Tasks::evict`]): bounded by the live set.
//! - The fold's clock is nanoseconds since the first frame; it never goes back.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::collections::{BTreeMap, VecDeque};

use blaze_protocol::now::{self, Fold, Look, MOST_TASKS, Memory, Now, State, TaskRecord, Worker};

use crate::{
    live::{EdgeRow, TaskRow},
    state::elapsed,
};

/// How long a settled task stays after its `end_ns`: 60 s, as the source keeps it.
pub const SETTLED_NS: i64 = 60_000_000_000;

/// The kind of a download's task.
pub const DOWNLOAD: &str = "download";

/// Samples of a download's bytes kept for its rate.
const SAMPLES: usize = 8;

/// The window a download's rate is measured over: 5 s.
const RATE_NS: i64 = 5_000_000_000;

/// A task: its process and its id there.
pub type Key = (u64, u64);

/// The live tasks and their fold.
#[derive(Debug, Default)]
pub struct Tasks {
    rows: BTreeMap<Key, TaskRow>,
    /// Each task's needs, by the source's ids, as its edges last said them.
    needs: BTreeMap<Key, Vec<u64>>,
    fold: Fold,
    ids: BTreeMap<Key, u64>,
    keys: BTreeMap<u64, Key>,
    next: u64,
    /// The first frame's `at_ns`: the fold's clock counts from it.
    origin: Option<i64>,
    /// The fold's clock as last advanced.
    clock_ns: u64,
    /// Tasks the fold holds that have left the live set.
    evicted: usize,
    /// New tasks past [`MOST_TASKS`] not kept.
    refused: u64,
    downloads: BTreeMap<Key, Samples>,
}

/// A download's last bytes, oldest first, and when they last moved.
#[derive(Debug, Default)]
struct Samples {
    at: VecDeque<(i64, u64)>,
    moved_ns: i64,
}

impl Samples {
    fn take(&mut self, at_ns: i64, done: u64) {
        if self.at.back().is_some_and(|&(_, had)| had == done) {
            return;
        }
        if self.at.len() == SAMPLES {
            self.at.pop_front();
        }
        self.at.push_back((at_ns, done));
        self.moved_ns = at_ns;
    }

    /// Bytes a second over the samples within [`RATE_NS`] of the last; `None` before two.
    fn rate(&self) -> Option<u64> {
        let &(last_ns, last) = self.at.back()?;
        let &(first_ns, first) =
            self.at.iter().find(|(at, _)| last_ns.saturating_sub(*at) <= RATE_NS)?;
        let ns = u128::try_from(last_ns.checked_sub(first_ns)?).ok().filter(|it| *it > 0)?;
        let bytes = u128::from(last.checked_sub(first)?);
        u64::try_from(bytes * 1_000_000_000 / ns).ok()
    }
}

impl Tasks {
    /// The task `key` names, as last said.
    pub fn row(&self, key: Key) -> Option<&TaskRow> {
        self.rows.get(&key)
    }

    /// The task the fold knows as `id`.
    pub fn key(&self, id: u64) -> Option<Key> {
        self.keys.get(&id).copied()
    }

    /// The fold's id of `key`, once it was said.
    pub fn fold_id(&self, key: Key) -> Option<u64> {
        self.ids.get(&key).copied()
    }

    /// Whether any task other than a download is live: a program with none is shown running.
    pub fn any(&self) -> bool {
        self.rows.values().any(|it| it.kind != DOWNLOAD)
    }

    /// New tasks past [`MOST_TASKS`] not kept, so far.
    pub fn refused(&self) -> u64 {
        self.refused
    }

    /// The fold's clock to `at_ns`.
    pub fn clock(&mut self, at_ns: i64) {
        let origin = *self.origin.get_or_insert(at_ns);
        self.clock_ns = self.clock_ns.max(elapsed(at_ns, origin));
        self.fold.advance_clock(self.clock_ns);
    }

    fn id(&mut self, key: Key) -> u64 {
        if let Some(&id) = self.ids.get(&key) {
            return id;
        }
        self.next += 1;
        self.ids.insert(key, self.next);
        self.keys.insert(self.next, key);
        self.next
    }

    /// `key`'s task to the fold, with the deps its edges name.
    fn take(&mut self, key: Key) {
        let Some(row) = self.rows.get(&key) else { return };
        // alloc: a task record the fold copies, once a task said or changed
        let (kind, name) = (row.kind.clone(), row.name.clone());
        let needs = self.needs.get(&key).map_or(&[][..], Vec::as_slice);
        // alloc: as above
        let needs: Vec<Key> = needs.iter().map(|&to| (key.0, to)).collect();
        let deps = needs.into_iter().map(|it| self.id(it)).collect();
        let id = self.id(key);
        self.fold.take(&TaskRecord { id, kind, name, deps });
    }

    /// A `tasks` frame's rows, at `at_ns`.
    pub fn upsert(&mut self, rows: &mut Vec<TaskRow>, at_ns: i64) {
        self.clock(at_ns);
        for row in rows.drain(..) {
            let key = (row.process, row.task);
            let had = self.rows.get(&key);
            if had.is_none() && self.rows.len() >= MOST_TASKS {
                self.refused = self.refused.saturating_add(1);
                continue;
            }
            if row.kind == DOWNLOAD {
                let samples = self.downloads.entry(key).or_default();
                samples.take(at_ns, row.progress_done.unwrap_or(0));
                self.rows.insert(key, row);
                continue;
            }
            let described = had.is_none_or(|it| it.kind != row.kind || it.name != row.name);
            let moved = match had {
                Some(it) => it.state != row.state,
                None => row.state != State::Waiting,
            };
            let state = row.state;
            self.rows.insert(key, row);
            if described {
                self.take(key);
            }
            if moved {
                let id = self.id(key);
                self.fold.moved(id, state);
            }
        }
    }

    /// A `task_edges` frame's rows, at `at_ns`: each task they name has these needs now.
    pub fn needs(&mut self, edges: &mut Vec<EdgeRow>, at_ns: i64) {
        self.clock(at_ns);
        // alloc: a frame's needs, by task
        let mut said: BTreeMap<Key, Vec<u64>> = BTreeMap::new();
        for edge in edges.drain(..) {
            // alloc: as above
            said.entry((edge.process, edge.from)).or_default().push(edge.to);
        }
        for (key, mut to) in said {
            to.sort_unstable();
            to.dedup();
            if self.needs.get(&key) == Some(&to) {
                continue;
            }
            self.needs.insert(key, to);
            self.take(key);
        }
    }

    /// Drops every task settled [`SETTLED_NS`] before `at_ns`, with its edges; past as many
    /// gone as live, the fold is made anew from the live ones.
    pub fn evict(&mut self, at_ns: i64) {
        let gone = |row: &TaskRow| {
            !row.state.unfinished()
                && row.end_ns.is_some_and(|end| end.saturating_add(SETTLED_NS) <= at_ns)
        };
        // alloc: the tasks that leave, once a frame that has some
        let leaving: Vec<Key> =
            self.rows.iter().filter(|(_, row)| gone(row)).map(|(key, _)| *key).collect();
        for key in &leaving {
            let row = self.rows.remove(key);
            self.needs.remove(key);
            self.downloads.remove(key);
            if let Some(id) = self.ids.remove(key) {
                self.keys.remove(&id);
            }
            if row.is_some_and(|it| it.kind != DOWNLOAD) {
                self.evicted += 1;
            }
        }
        if self.evicted > 0 && self.evicted > self.rows.len() {
            self.fold = Fold::new();
            self.fold.advance_clock(self.clock_ns);
            self.evicted = 0;
            // alloc: the live tasks' keys, once a remake
            let live: Vec<(Key, State)> = self
                .rows
                .iter()
                .filter(|(_, row)| row.kind != DOWNLOAD)
                .map(|(key, row)| (*key, row.state))
                .collect();
            for (key, state) in live {
                self.take(key);
                if state != State::Waiting {
                    let id = self.id(key);
                    self.fold.moved(id, state);
                }
            }
        }
    }

    /// The program as of `now_ns`, with `threads` the workers (`workers` of them) and `memory`
    /// the program's.
    pub fn look(
        &mut self,
        now_ns: i64,
        workers: u32,
        threads: &[Worker],
        memory: &Memory,
    ) -> Now {
        self.clock(now_ns);
        let ms = |ns: u64| ns / 1_000_000;
        let downloads: Vec<now::Download> = self
            .rows
            .iter()
            .filter(|(_, row)| row.kind == DOWNLOAD && row.state.unfinished())
            .map(|(key, row)| {
                let samples = self.downloads.get(key);
                now::Download {
                    // clone: the look's answer owns its strings
                    what: row.name.clone(),
                    done: row.progress_done.unwrap_or(0),
                    total: row.progress_total,
                    rate: samples.and_then(Samples::rate),
                    idle_ms: ms(elapsed(now_ns, samples.map_or(now_ns, |it| it.moved_ns))),
                    elapsed_ms: ms(elapsed(now_ns, row.start_ns.unwrap_or(now_ns))),
                }
            })
            // alloc: the look's downloads, a handful
            .collect();
        // The daemon's stall line lives in the daemon (its loop watchdog); a profile's reader
        // finds its stalls as `blaze::stall` events instead.
        let look = Look {
            workers,
            threads,
            downloads: &downloads,
            stage: "",
            stall: None,
            memory,
            expanders: &[],
            expansion_threads: 0,
        };
        self.fold.look(&look)
    }
}
