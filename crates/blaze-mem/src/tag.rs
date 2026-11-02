//! Counters per subsystem (docs/v2/plan/design/memory.md "Budgets and visibility").
//!
//! Every allocator carries a [`Tag`]. Per tag this keeps the bytes it holds now (`live`), the
//! most it held (`high_water`) and the bytes it handed out in all (`allocated`, whose rate is
//! the allocation rate: [`Rate`]). Memory admission (`blaze_graph::admission`, lane core) reads
//! [`snapshot`] and [`over_budget`]; the agent API exposes the snapshot.
//!
//! Counters move at coarse grain only, never per allocation: a region's commit, a chunk taken
//! or given back, a pool's batch of 64, a frame's task ending. Each tag's counters sit on their
//! own 128-byte line, so subsystems never write each other's.

use std::{cell::Cell, fmt};

// `std`'s even under `cfg(blaze_check)`: the counters are process-wide statics every test
// shares, and a model execution restarts a checker atomic from its initial value and writes
// its own newest value back, which would drop the bytes of regions alive outside the model.
use std::sync::atomic::{AtomicU64, Ordering};

/// The subsystem memory is held for.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum Tag {
    Parse,
    Nameres,
    Expand,
    Infer,
    Mir,
    Codegen,
    Link,
    Store,
    Vfs,
    Usercode,
    /// Lints and diagnostics passes.
    Diagnostics,
    /// The query database's memo tables and runs.
    Query,
    /// The plane's tasks and requests.
    Plane,
    /// The core's I/O operations, timers and watch slots.
    Io,
    /// Interned names, paths and types.
    Intern,
    /// Workers' frames: memory a task borrows and gives back when it ends.
    Frames,
    /// Chunks cached for arenas and not held by any.
    Chunks,
    Api,
    /// The fuzzer's corpus and its jobs.
    Fuzz,
    Other,
}

impl Tag {
    pub const COUNT: usize = 20;

    pub const ALL: [Tag; Tag::COUNT] = [
        Tag::Parse,
        Tag::Nameres,
        Tag::Expand,
        Tag::Infer,
        Tag::Mir,
        Tag::Codegen,
        Tag::Link,
        Tag::Store,
        Tag::Vfs,
        Tag::Usercode,
        Tag::Diagnostics,
        Tag::Query,
        Tag::Plane,
        Tag::Io,
        Tag::Intern,
        Tag::Frames,
        Tag::Chunks,
        Tag::Api,
        Tag::Fuzz,
        Tag::Other,
    ];

    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn name(self) -> &'static str {
        match self {
            Tag::Parse => "parse",
            Tag::Nameres => "nameres",
            Tag::Expand => "expand",
            Tag::Infer => "infer",
            Tag::Mir => "mir",
            Tag::Codegen => "codegen",
            Tag::Link => "link",
            Tag::Store => "store",
            Tag::Vfs => "vfs",
            Tag::Usercode => "usercode",
            Tag::Diagnostics => "diagnostics",
            Tag::Query => "query",
            Tag::Plane => "plane",
            Tag::Io => "io",
            Tag::Intern => "intern",
            Tag::Frames => "frames",
            Tag::Chunks => "chunks",
            Tag::Api => "api",
            Tag::Fuzz => "fuzz",
            Tag::Other => "other",
        }
    }

    /// The tag of a name [`Tag::name`] gives; `None` for any other.
    pub fn from_name(name: &str) -> Option<Tag> {
        Tag::ALL.into_iter().find(|tag| tag.name() == name)
    }
}

const _: () = {
    let mut i = 0;
    while i < Tag::COUNT {
        assert!(Tag::ALL[i] as usize == i, "Tag::ALL is in discriminant order");
        i += 1;
    }
};

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[repr(align(128))]
struct Line {
    live: AtomicU64,
    high_water: AtomicU64,
    allocated: AtomicU64,
    /// The most one task of this tag took from its frame.
    task_peak: AtomicU64,
    /// What its tasks' frames copied to grow a block ([`Copied`]).
    grow_copies: AtomicU64,
    grow_copied: AtomicU64,
    budget: AtomicU64,
}

impl Line {
    const fn new() -> Line {
        Line {
            live: AtomicU64::new(0),
            high_water: AtomicU64::new(0),
            allocated: AtomicU64::new(0),
            task_peak: AtomicU64::new(0),
            grow_copies: AtomicU64::new(0),
            grow_copied: AtomicU64::new(0),
            budget: AtomicU64::new(u64::MAX),
        }
    }
}

static LINES: [Line; Tag::COUNT] = [const { Line::new() }; Tag::COUNT];

#[repr(align(128))]
struct Committed {
    now: AtomicU64,
    high_water: AtomicU64,
}

/// Bytes committed in every region, whatever holds them.
static COMMITTED: Committed =
    Committed { now: AtomicU64::new(0), high_water: AtomicU64::new(0) };

fn line(tag: Tag) -> &'static Line {
    &LINES[tag.index()]
}

fn as_u64(bytes: usize) -> u64 {
    bytes as u64
}

/// `tag` holds `bytes` more.
pub fn add(tag: Tag, bytes: usize) {
    if bytes == 0 {
        return;
    }
    let line = line(tag);
    let now = line.live.fetch_add(as_u64(bytes), Ordering::Relaxed) + as_u64(bytes);
    raise(&line.high_water, now);
}

/// `high_water.fetch_max(now)`, with the read-modify-write only when `now` passes what it
/// holds: the mark never falls, so a load that sees it at or past `now` is the same answer, and
/// in steady state it costs a load of a line the caller's add just made its own.
#[inline]
fn raise(high_water: &AtomicU64, now: u64) {
    if now > high_water.load(Ordering::Relaxed) {
        high_water.fetch_max(now, Ordering::Relaxed);
    }
}

/// `tag` holds `bytes` fewer.
pub fn sub(tag: Tag, bytes: usize) {
    if bytes != 0 {
        line(tag).live.fetch_sub(as_u64(bytes), Ordering::Relaxed);
    }
}

/// `bytes` held by `from` are now held by `to` (a cached chunk taken by an arena, say).
pub fn transfer(from: Tag, to: Tag, bytes: usize) {
    if from != to {
        sub(from, bytes);
        add(to, bytes);
    }
}

/// `tag` handed out `bytes` more (the source of its allocation rate).
pub fn allocated(tag: Tag, bytes: usize) {
    if bytes != 0 {
        line(tag).allocated.fetch_add(as_u64(bytes), Ordering::Relaxed);
    }
}

/// A task's bytes a thread adds to a tag's `allocated` before folding them into the tag's line.
const FOLD_AT: u64 = 1 << 20;

/// What the thread's tasks added to each tag's `allocated` and is not on the tag's line yet:
/// folded in at [`FOLD_AT`], at a [`snapshot`] the thread takes, and when the thread ends.
struct Pending([Cell<u64>; Tag::COUNT]);

impl Pending {
    fn fold(&self) {
        for (pending, line) in self.0.iter().zip(&LINES) {
            let bytes = pending.replace(0);
            if bytes != 0 {
                line.allocated.fetch_add(bytes, Ordering::Relaxed);
            }
        }
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.fold();
    }
}

thread_local! {
    static PENDING: Pending = const { Pending([const { Cell::new(0) }; Tag::COUNT]) };
}

/// What a frame copied to grow blocks: a block that grows while it is not the frame's last
/// (another allocation came after it) moves to a new one, copied whole. A frame counts these;
/// a block growing in place costs nothing and is not counted.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Copied {
    /// Blocks moved.
    pub copies: u64,
    /// Bytes copied moving them.
    pub bytes: u64,
}

thread_local! {
    /// What the frames of the thread's ended tasks copied, never decreasing.
    static COPIED: Cell<Copied> = const { Cell::new(Copied { copies: 0, bytes: 0 }) };
}

/// What the frames of the calling thread's ended tasks copied so far, never decreasing: a
/// span's part is the difference across it ([`task_ended`] adds a task's at its end).
pub fn copied_on_thread() -> Copied {
    COPIED.try_with(Cell::get).unwrap_or_default()
}

/// A task of `tag` took `peak` bytes of its frame at most, all given back now, and its frame
/// copied `copied` to grow blocks. Warm, it writes no shared line: the peak is written only
/// when it rises, the bytes gather per thread, and copies, which only a task that moved a
/// block has, are added to the tag's line.
pub fn task_ended(tag: Tag, peak: usize, copied: Copied) {
    if copied.copies != 0 {
        let line = line(tag);
        line.grow_copies.fetch_add(copied.copies, Ordering::Relaxed);
        line.grow_copied.fetch_add(copied.bytes, Ordering::Relaxed);
        _ = COPIED.try_with(|it| {
            let was = it.get();
            it.set(Copied { copies: was.copies + copied.copies, bytes: was.bytes + copied.bytes });
        });
    }
    if peak == 0 {
        return;
    }
    let line = line(tag);
    let peak = as_u64(peak);
    if peak > line.task_peak.load(Ordering::Relaxed) {
        line.task_peak.fetch_max(peak, Ordering::Relaxed);
    }
    let full = PENDING.try_with(|pending| {
        let gathered = &pending.0[tag.index()];
        let bytes = gathered.get() + peak;
        if bytes < FOLD_AT {
            gathered.set(bytes);
            return 0;
        }
        gathered.set(0);
        bytes
    });
    // A thread being torn down has no pending counts left: the bytes go straight to the line.
    let bytes = full.unwrap_or(peak);
    if bytes != 0 {
        line.allocated.fetch_add(bytes, Ordering::Relaxed);
    }
}

pub(crate) fn committed(bytes: usize) {
    let now = COMMITTED.now.fetch_add(as_u64(bytes), Ordering::Relaxed) + as_u64(bytes);
    raise(&COMMITTED.high_water, now);
}

pub(crate) fn decommitted(bytes: usize) {
    COMMITTED.now.fetch_sub(as_u64(bytes), Ordering::Relaxed);
}

/// The most `tag` should hold; `u64::MAX` (no limit) until set.
pub fn set_budget(tag: Tag, bytes: u64) {
    line(tag).budget.store(bytes, Ordering::Relaxed);
}

pub fn budget(tag: Tag) -> u64 {
    line(tag).budget.load(Ordering::Relaxed)
}

/// Whether `tag` holds more than its budget: admission throttles its tasks before the
/// machine runs short.
pub fn over_budget(tag: Tag) -> bool {
    let line = line(tag);
    line.live.load(Ordering::Relaxed) > line.budget.load(Ordering::Relaxed)
}

/// What `tag` may still take before it is over budget.
pub fn headroom(tag: Tag) -> u64 {
    let line = line(tag);
    line.budget.load(Ordering::Relaxed).saturating_sub(line.live.load(Ordering::Relaxed))
}

/// Bytes committed in every region now: [`Snapshot::committed`] without the rest of a
/// snapshot, one load.
pub fn committed_now() -> u64 {
    COMMITTED.now.load(Ordering::Relaxed)
}

/// One tag's counters at one moment.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Counters {
    pub live: u64,
    pub high_water: u64,
    pub allocated: u64,
    pub task_peak: u64,
    /// Blocks its tasks' frames moved to grow them, and the bytes that copied ([`Copied`]).
    pub grow_copies: u64,
    pub grow_copied: u64,
    pub budget: u64,
}

/// Every tag's counters, read with relaxed loads: each number is exact, the set is not one
/// instant (no counter is written under a lock, so none can be).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Snapshot {
    pub tags: [Counters; Tag::COUNT],
    /// Bytes committed in every region now, and at most.
    pub committed: u64,
    pub committed_high_water: u64,
}

impl Snapshot {
    pub fn get(&self, tag: Tag) -> Counters {
        self.tags[tag.index()]
    }

    /// What every tag holds, summed.
    pub fn live(&self) -> u64 {
        self.tags.iter().map(|c| c.live).sum()
    }
}

/// The counters now. Another thread's tasks' bytes reach `allocated` up to [`FOLD_AT`] per tag
/// late; the calling thread's are folded in first.
pub fn snapshot() -> Snapshot {
    let _ = PENDING.try_with(Pending::fold);
    let mut tags = [Counters::default(); Tag::COUNT];
    for (counters, line) in tags.iter_mut().zip(&LINES) {
        *counters = Counters {
            live: line.live.load(Ordering::Relaxed),
            high_water: line.high_water.load(Ordering::Relaxed),
            allocated: line.allocated.load(Ordering::Relaxed),
            task_peak: line.task_peak.load(Ordering::Relaxed),
            grow_copies: line.grow_copies.load(Ordering::Relaxed),
            grow_copied: line.grow_copied.load(Ordering::Relaxed),
            budget: line.budget.load(Ordering::Relaxed),
        };
    }
    Snapshot {
        tags,
        committed: COMMITTED.now.load(Ordering::Relaxed),
        committed_high_water: COMMITTED.high_water.load(Ordering::Relaxed),
    }
}

/// Allocation rate per tag, from successive snapshots. The caller gives the time (from its
/// clock effect), so the rate of a replayed run is the same as the run's.
#[derive(Clone, Debug, Default)]
pub struct Rate {
    last: Option<(u64, [u64; Tag::COUNT])>,
}

impl Rate {
    pub fn new() -> Rate {
        Rate::default()
    }

    /// Bytes per second each tag handed out since the last sample, `now_ns` in nanoseconds of
    /// a monotonic clock; zeros for the first sample or when no time passed.
    pub fn sample(&mut self, now_ns: u64, snapshot: &Snapshot) -> [u64; Tag::COUNT] {
        let mut allocated = [0; Tag::COUNT];
        for (out, counters) in allocated.iter_mut().zip(&snapshot.tags) {
            *out = counters.allocated;
        }
        let mut rate = [0; Tag::COUNT];
        if let Some((then, before)) = self.last
            && now_ns > then
        {
            let elapsed = u128::from(now_ns - then);
            for ((out, now), before) in rate.iter_mut().zip(allocated).zip(before) {
                let bytes = u128::from(now.saturating_sub(before));
                *out = u64::try_from(bytes * 1_000_000_000 / elapsed).unwrap_or(u64::MAX);
            }
        }
        self.last = Some((now_ns, allocated));
        rate
    }
}
