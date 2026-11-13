//! What a task is: its kind, the subsystem it charges, and the one record admission forecasts
//! its memory from (docs/v2/plan/design/end-state/memory.md, section 4.3).
//!
//! Admission's forecast is for throughput only: a task starts when a thread of its pool is free
//! and its forecast fits what the gate (`blaze_mem::budget::Budget`) leaves free, and a low
//! forecast costs a wound, never the ceiling, since every commit passes the gate. So there is
//! one predictor, [`Peaks`]: the peak a task of a kind and input size class last committed,
//! written at the task's end, read at its push; one never seen is one quantum. A task at or
//! under [`STANDING`] (its worker's standing frame) takes no reservation and never waits for
//! memory.
//!
//! A task's peak is everything it held: what its work allocated through the global allocator,
//! on its own job and on every job of the pool scopes it opened (its memory account,
//! `arena::account`), never a sample of the whole process, and what its frames held
//! (`blaze_mem::frame`). Each kind belongs to a subsystem ([`tag_of`]), the tag its memory is
//! named by in spans and `blaze explain memory`.

#![forbid(unsafe_code)]

use std::fmt;

use blaze_format::cursor::{self, Put as _, Reader};
use blaze_hash::HashMap;
use blaze_mem::{Tag, budget::QUANTUM};
use serde::{Deserialize, Serialize};
use std::alloc::Global;

/// A kind of task whose tasks are alike in what they hold: a query kind's number
/// (`blaze_query::QueryKind`), or one of the plane's own (a link, a test run).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(transparent)]
pub struct TaskKind(pub u32);

/// The daemon's own task kinds, past every query kind (a `u16`) but the load's.
pub mod kinds {
    use super::TaskKind;

    /// The load's `Plan`: its query kind (`blaze-units`).
    pub const PLAN: TaskKind = TaskKind(0x6000);
    /// What a build fetches into the store.
    pub const FETCH: TaskKind = TaskKind(0x1_0001);
    /// The standard library built.
    pub const LIBRARY: TaskKind = TaskKind(0x1_0002);
    /// A crate's bodies lowered to MIR.
    pub const LOWER: TaskKind = TaskKind(0x1_0003);
    /// A unit's codegen: its functions compiled from its roots.
    pub const COMPILE: TaskKind = TaskKind(0x1_0004);
    /// What a binary reaches, for its compiles.
    pub const REACH: TaskKind = TaskKind(0x1_0005);
    /// A view's queries walked for `Trace`, `Verify` or `CodegenInputs`.
    pub const WALK: TaskKind = TaskKind(0x1_0006);
    /// `blaze test --ub`: the tests selected, and one test evaluated.
    pub const UB_TESTS: TaskKind = TaskKind(0x1_0007);
    pub const EVALUATE: TaskKind = TaskKind(0x1_0008);
    /// A program's codegen: of a binary, test, bench, example or `cdylib`.
    pub const PROGRAM: TaskKind = TaskKind(0x1_0009);
    /// A unit's names: whether its record is kept, its crate's def map, its check's bodies.
    pub const UNIT_NAMES: TaskKind = TaskKind(0x1_000a);
    /// A unit's check: the diagnostics of every body of its crate.
    pub const UNIT_BODIES: TaskKind = TaskKind(0x1_000b);
    /// A unit's link: its images linked from its codegen, a proc macro's expanders loaded.
    pub const UNIT_LINK: TaskKind = TaskKind(0x1_000c);
    /// A unit's outputs hashed, and its record encoded, for the store.
    pub const UNIT_STORE: TaskKind = TaskKind(0x1_000d);
    /// A program's link.
    pub const PROGRAM_LINK: TaskKind = TaskKind(0x1_000e);
    /// What a build left for the store (dependency images, check outcomes, goals) written from
    /// its front end as it is kept, before the build answers.
    pub const STORE_KEPT: TaskKind = TaskKind(0x1_000f);
    /// A kept front end's values given back because memory is short: swept on a worker, never
    /// the loop. Pushed with no memory of its own, since it runs because memory is short.
    pub const GIVE_BACK: TaskKind = TaskKind(0x1_0010);
    /// A checkout's front-end carry (`blaze_cargo::serve::front_carry`): its kernel session
    /// carried to a build's pin and saved, on a worker after the build, never the loop.
    pub const FRONT_CARRY: TaskKind = TaskKind(0x1_0011);
}

/// The name of one of the daemon's own task kinds ([`kinds`]), as a profile's `task` span
/// says it; `None` for a query kind, which its query's name says.
pub const fn name_of(kind: TaskKind) -> Option<&'static str> {
    Some(match kind {
        kinds::PLAN => "plan",
        kinds::FETCH => "fetch",
        kinds::LIBRARY => "library",
        kinds::LOWER => "lower",
        kinds::COMPILE => "compile",
        kinds::REACH => "reach",
        kinds::WALK => "walk",
        kinds::UB_TESTS => "ub tests",
        kinds::EVALUATE => "evaluate",
        kinds::PROGRAM => "program",
        kinds::UNIT_NAMES => "unit names",
        kinds::UNIT_BODIES => "unit bodies",
        kinds::UNIT_LINK => "unit link",
        kinds::UNIT_STORE => "unit store",
        kinds::PROGRAM_LINK => "program link",
        kinds::STORE_KEPT => "store kept",
        kinds::GIVE_BACK => "give back",
        kinds::FRONT_CARRY => "front carry",
        _ => return None,
    })
}

/// The subsystem whose memory a task of `kind` holds: the tag it is named by.
///
/// Query kinds go by `blaze_query::kinds`' ranges (inputs, facts `0x1000`, the front end
/// `0x2000`, codegen and link `0x4000`, user code `0x5000`, the units' loads `0x6000`), the
/// front end's and the back end's ranges split by subsystem as written here and in
/// docs/v2/plan/design/memory.md ("Kinds and tags"); the daemon's own by [`kinds`]. A kind
/// none of them names is [`Tag::Other`]'s.
pub const fn tag_of(kind: TaskKind) -> Tag {
    match kind {
        kinds::FETCH | kinds::UNIT_STORE | kinds::STORE_KEPT => Tag::Store,
        kinds::LIBRARY | kinds::COMPILE | kinds::REACH | kinds::PROGRAM => Tag::Codegen,
        kinds::UNIT_NAMES => Tag::Nameres,
        kinds::FRONT_CARRY => Tag::Parse,
        kinds::UNIT_BODIES => Tag::Infer,
        kinds::UNIT_LINK | kinds::PROGRAM_LINK => Tag::Link,
        kinds::LOWER | kinds::UB_TESTS | kinds::EVALUATE => Tag::Mir,
        kinds::WALK | kinds::GIVE_BACK | TaskKind(4) => Tag::Query,
        TaskKind(1..=3) | TaskKind(5..=6) => Tag::Vfs,
        TaskKind(0x1000..0x2000) => Tag::Nameres,
        TaskKind(0x2000..0x2200) => Tag::Parse,
        TaskKind(0x2200..0x2400) => Tag::Nameres,
        TaskKind(0x2400..0x2c00) => Tag::Infer,
        TaskKind(0x2c00..0x3000) => Tag::Expand,
        TaskKind(0x3000..0x4000) => Tag::Mir,
        TaskKind(0x4000..0x4800) => Tag::Codegen,
        TaskKind(0x4800..0x5000) => Tag::Link,
        TaskKind(0x5000..0x6000) => Tag::Usercode,
        TaskKind(0x6000..0x7000) => Tag::Vfs,
        _ => Tag::Other,
    }
}

/// Runs `f` as a task of `kind`: in a [`blaze_mem::frame::task`] of [`tag_of`]`(kind)`, so
/// what it allocates from the global heap is charged to that subsystem's line and not to the
/// heap outside any task, and says the most it held: its global allocations, its fan-out's
/// included ([`crate::arena::account`]), and its frames' together, the peak [`Peaks`] record.
/// Every runner of tasks (the exec scheduler's, the daemon's plane) runs a task's work through
/// this, so a task's memory has one home whoever runs it. Says its global-heap churn too
/// ([`TaskSpent::heap`]), for its `task` span. An unranked task ([`blaze_mem::frame::task`]):
/// one its scheduler does not post again after it gives way; [`run_ranked`] is the other.
pub fn run_as<R>(kind: TaskKind, f: impl FnOnce() -> R) -> (R, TaskSpent) {
    let tag = tag_of(kind);
    let ((out, heap), frames) = blaze_mem::frame::measure(|| {
        crate::arena::measured(|| blaze_mem::frame::task(tag, |_| f()))
    });
    (out, TaskSpent { peak: heap.peak + frames as u64, heap })
}

/// [`run_as`] at the gate's rank its scheduler gave it ([`blaze_mem::frame::task_ranked`]: its
/// band and age, kept when it is posted again after giving way). It never waits to start:
/// while the gate holds it behind an elder or the room, it says
/// [`blaze_mem::gate::HeldBack`] at once and `f` does not run; its scheduler tells its plane
/// it gave way held (`PlaneInput::Yielded { held: true }`) and posts it again once
/// [`blaze_mem::gate::Gate::take_unheld`] names it (`PlaneInput::Unheld`). A thread is never
/// spent waiting at the gate.
pub fn run_ranked<R>(
    kind: TaskKind,
    rank: blaze_mem::gate::Ranked,
    f: impl FnOnce() -> R,
) -> Result<(R, TaskSpent), blaze_mem::gate::HeldBack> {
    let tag = tag_of(kind);
    let ((out, heap), frames) = blaze_mem::frame::measure(|| {
        crate::arena::measured(|| blaze_mem::frame::task_ranked(tag, rank, |_| f()))
    });
    let out = out?;
    Ok((out, TaskSpent { peak: heap.peak + frames as u64, heap }))
}

/// What a task spent ([`run_as`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TaskSpent {
    /// The most it held at once: its global allocations, its fan-out's included, and its
    /// frames' together.
    pub peak: u64,
    /// Its global allocations alone ([`crate::arena::Spent`]): their peak, and what it
    /// allocated and freed in all.
    pub heap: crate::arena::Spent,
}

/// A worker's standing frame (memory.md 4.1): a task forecast at or under it takes no
/// reservation, so it never waits for memory (a lookup never sees admission).
pub const STANDING: u64 = 8 << 20;

/// What admission reserves for a task forecast to hold `bytes`: whole quanta of the gate
/// ([`QUANTUM`]), and nothing at or under [`STANDING`].
pub const fn forecast_of(bytes: u64) -> u64 {
    let whole = bytes.div_ceil(QUANTUM).saturating_mul(QUANTUM);
    if whole <= STANDING { 0 } else { whole }
}

/// What the gate leaves free for new starts, for [`crate::plane::PlaneInput::Room`]: its limit,
/// less what it counts committed, less the overdrafts it sets aside for the workers, less
/// `still_forecast`: what the running tasks are forecast to take past what they hold now (the
/// sum over them of their forecast less their held quanta, never below 0). Without the last
/// two a start could be forecast into room an earlier start has not reached yet. An overdraft
/// in use is counted twice (committed and set aside), so it errs low, never high.
pub fn free_for_starts(budget: &blaze_mem::budget::Budget, still_forecast: u64) -> u64 {
    let config = budget.config();
    let overdrafts = config.overdraft_per_worker.saturating_mul(u64::from(config.workers));
    budget.limit().saturating_sub(
        budget.committed().saturating_add(overdrafts).saturating_add(still_forecast),
    )
}

/// The size class of an input of `size` (whatever its kind counts: a unit's `reached`
/// functions, a file's bytes): its bit length, so each class is twice the one below.
pub const fn class_of(size: u64) -> u8 {
    (u64::BITS - size.leading_zeros()) as u8
}

/// The first bytes of [`Peaks::encode`]'s bytes.
const PEAKS_MAGIC: &[u8; 8] = b"blzpeaks";
/// The layout of [`Peaks::encode`]'s bytes, after the magic.
pub const PEAKS_LAYOUT: u64 = 1;
/// The magic of what the daemon's image held before [`Peaks`]: the per-kind estimates, read as
/// layout 0.
const ESTIMATES_MAGIC: &[u8; 8] = b"blzest01";
/// The bytes of one entry: its kind, its class and its peak.
const ENTRY_BYTES: usize = 8 + 1 + 8;

/// The one predictor of admission (memory.md 4.3): for each kind of task and size class of its
/// input, the peak bytes the last task of them committed. Keyed per kind and class, never per
/// crate (rule 65): every crate's tasks of a kind and size teach every other's. Kept across
/// runs in the daemon's image ([`Peaks::encode`]).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Peaks {
    by: HashMap<(u64, u8), u64, Global>, // alloc: the record, kept for the daemon's life and persisted
}

impl Peaks {
    pub fn new() -> Peaks {
        Peaks::default()
    }

    /// The peak the last task of `kind` and `class` committed, if one was recorded.
    pub fn peak(&self, kind: u64, class: u8) -> Option<u64> {
        self.by.get(&(kind, class)).copied()
    }

    /// A task of `kind` and `class` ended having committed `peak` bytes at most: the record
    /// from now on, whatever it was.
    pub fn observe(&mut self, kind: u64, class: u8, peak: u64) {
        self.by.insert((kind, class), peak);
    }

    /// What a task of `kind` and `class` is forecast to hold: the recorded peak, else one
    /// quantum.
    pub fn forecast(&self, kind: u64, class: u8) -> u64 {
        self.peak(kind, class).unwrap_or(QUANTUM)
    }

    pub fn len(&self) -> usize {
        self.by.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by.is_empty()
    }

    /// The record as bytes for the daemon's image: the same record gives the same bytes,
    /// whatever order it was taught in. Little-endian: the magic `blzpeaks`, [`PEAKS_LAYOUT`],
    /// the count of entries, then each entry by `(kind, class)` ascending: its kind (`u64`),
    /// class (`u8`) and peak (`u64`).
    pub fn encode(&self) -> Vec<u8> {
        let mut entries: Vec<((u64, u8), u64)> = // alloc: the entries in order, once per encode
            self.by.iter().map(|(&key, &peak)| (key, peak)).collect();
        entries.sort_unstable_by_key(|it| it.0);
        // alloc: the image's bytes, once per encode
        let mut bytes = Vec::with_capacity(PEAKS_MAGIC.len() + 16 + entries.len() * ENTRY_BYTES);
        bytes.put_bytes(PEAKS_MAGIC);
        bytes.put_u64(PEAKS_LAYOUT);
        bytes.put_u64(entries.len() as u64);
        for ((kind, class), peak) in entries {
            bytes.put_u64(kind);
            bytes.put_u8(class);
            bytes.put_u64(peak);
        }
        bytes
    }

    /// The record [`Peaks::encode`] gave `bytes`, or what is wrong with them, named: another
    /// file's bytes, another layout's (the estimates before it are layout 0), cut short or with
    /// more after, or entries out of order.
    pub fn decode(bytes: &[u8]) -> Result<Peaks, PeaksError> {
        let mut read = Reader::new(bytes);
        let magic = *read.array::<8>()?;
        if &magic == ESTIMATES_MAGIC {
            return Err(PeaksError::Layout { found: 0, reads: PEAKS_LAYOUT });
        }
        if &magic != PEAKS_MAGIC {
            return Err(PeaksError::Magic { found: magic });
        }
        let layout = read.u64()?;
        if layout != PEAKS_LAYOUT {
            return Err(PeaksError::Layout { found: layout, reads: PEAKS_LAYOUT });
        }
        let count = read.u64()?;
        if count.checked_mul(ENTRY_BYTES as u64) != Some(read.left() as u64) {
            return Err(PeaksError::Length { count, bytes: bytes.len() });
        }
        let mut by = HashMap::new_in(Global); // alloc: the record, kept for the daemon's life
        by.reserve(count as usize);
        let mut last: Option<(u64, u8)> = None;
        for _ in 0..count {
            let key = (read.u64()?, read.u8()?);
            if last.is_some_and(|it| it >= key) {
                return Err(PeaksError::Order { kind: key.0, class: key.1 });
            }
            last = Some(key);
            by.insert(key, read.u64()?);
        }
        Ok(Peaks { by })
    }
}

/// Why bytes are not a [`Peaks`] record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeaksError {
    /// Not a peaks record at all.
    Magic { found: [u8; 8] },
    /// A peaks record of layout `found`, where this blaze reads layout `reads`: learned again
    /// from the next tasks' peaks.
    Layout { found: u64, reads: u64 },
    Short { at: usize },
    /// Not as many bytes as `count` entries take.
    Length { count: u64, bytes: usize },
    /// An entry not after the one before it.
    Order { kind: u64, class: u8 },
}

impl fmt::Display for PeaksError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            PeaksError::Magic { found } => {
                write!(f, "memory peaks: the bytes begin {found:02x?}, not `blzpeaks`")
            }
            PeaksError::Layout { found, reads } => write!(
                f,
                "memory peaks of layout {found}, where this blaze reads layout {reads}: they are \
                 learned again from the next tasks' peaks"
            ),
            PeaksError::Short { at } => write!(f, "memory peaks cut short at byte {at}"),
            PeaksError::Length { count, bytes } => {
                write!(f, "memory peaks of {count} entries in {bytes} bytes")
            }
            PeaksError::Order { kind, class } => {
                write!(f, "memory peaks with kind {kind:#x} class {class} out of order")
            }
        }
    }
}

impl std::error::Error for PeaksError {}

impl From<cursor::Error> for PeaksError {
    fn from(error: cursor::Error) -> PeaksError {
        PeaksError::Short { at: error.at }
    }
}

/// What a node is predicted to cost before it runs: its [`Spec::cost`](crate::Spec::cost) for
/// the plane's critical-path order, when the predictor knows one, and its peak bytes for
/// admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prediction {
    /// The node's cost, in the predictor's one unit for every node of a plane; `None` keeps
    /// the spec's.
    pub cost: Option<u64>,
    /// The most bytes it is predicted to hold.
    pub peak: u64,
}

impl Prediction {
    /// `spec` with the prediction in: its memory never under the prediction's peak, its cost
    /// the prediction's when it has one.
    pub fn apply(self, spec: &mut crate::Spec) {
        spec.memory = spec.memory.max(self.peak);
        if let Some(cost) = self.cost {
            spec.cost = cost;
        }
    }
}

/// What the plane asks for a node's [`Prediction`] and tells what the node then did. A
/// prediction orders and admits work only: it never changes what is built.
pub trait Predict: Send {
    /// What a task of `kind` with `key` is predicted to cost: its key's own record, or, with
    /// none, its kind's prediction (what one ask with no key says), so one ask a node is enough.
    fn predict(&self, kind: TaskKind, key: Option<crate::Key>) -> Prediction;
    /// A task of `kind` with `key` ran for `ran_ns` of its own (not its waits) and held `peak`
    /// bytes at most.
    fn observe(&mut self, kind: TaskKind, key: Option<crate::Key>, ran_ns: u64, peak: u64);
    /// What it learned, as bytes for the daemon's image.
    fn encode(&self) -> Vec<u8>;
    /// Whether it has measured a task of `kind` with `key` itself, not only others of its
    /// kind. A predictor that keeps no keys says every task is.
    fn measured(&self, _kind: TaskKind, _key: Option<crate::Key>) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use blaze_ids::rng::SplitMix64;

    use super::*;

    /// Each range of `blaze_query::kinds` and each kind in use goes to its subsystem: the
    /// inputs, the facts, name resolution's kinds (`hir-def`'s `0x2200..`), codegen's
    /// (`blaze-codegen`'s `0x4000..`), the units' loads and the daemon's own.
    #[test]
    fn each_kind_belongs_to_its_subsystem() {
        let cases = [
            (1, Tag::Vfs),
            (3, Tag::Vfs),
            (4, Tag::Query),
            (5, Tag::Vfs),
            (6, Tag::Vfs),
            (0x1000, Tag::Nameres),
            (0x1fff, Tag::Nameres),
            (0x2000, Tag::Parse),
            (0x2200, Tag::Nameres),
            (0x2217, Tag::Nameres),
            (0x2400, Tag::Infer),
            (0x2c00, Tag::Expand),
            (0x3000, Tag::Mir),
            (0x3fff, Tag::Mir),
            (0x4000, Tag::Codegen),
            (0x4006, Tag::Codegen),
            (0x4800, Tag::Link),
            (0x5000, Tag::Usercode),
            (0x6000, Tag::Vfs),
            (0x601d, Tag::Vfs),
            (0x1_0001, Tag::Store),
            (0x1_0004, Tag::Codegen),
            (0x1_0008, Tag::Mir),
            (0x1_0009, Tag::Codegen),
            (0x1_000a, Tag::Nameres),
            (0x1_000b, Tag::Infer),
            (0x1_000c, Tag::Link),
            (0x1_000d, Tag::Store),
            (0x1_000e, Tag::Link),
            (0, Tag::Other),
            (0x7000, Tag::Other),
            (u32::MAX, Tag::Other),
        ];
        for (kind, tag) in cases {
            assert_eq!(tag_of(TaskKind(kind)), tag, "kind {kind:#x}");
        }
        assert_eq!(tag_of(kinds::PLAN), Tag::Vfs);
        assert_eq!(tag_of(kinds::WALK), Tag::Query);
    }

    /// A record taught in a drawn order: kinds and classes from a small range, so keys repeat
    /// and the latest peak must win.
    fn taught(seed: u64, lessons: usize) -> (Peaks, Vec<((u64, u8), u64)>) {
        let mut random = SplitMix64::new(seed);
        let mut peaks = Peaks::new();
        let mut lessons_given = Vec::with_capacity(lessons);
        for _ in 0..lessons {
            let key = (random.below(40) + 0x4000, random.below(24) as u8);
            let peak = random.below(40 << 30);
            peaks.observe(key.0, key.1, peak);
            lessons_given.push((key, peak));
        }
        (peaks, lessons_given)
    }

    /// The invariant: a record is the latest peak of each kind and class, one never seen is
    /// forecast one quantum, and admission's forecast is whole quanta, nothing at or under the
    /// standing frame.
    #[test]
    fn the_latest_peak_is_the_forecast_and_unseen_is_one_quantum() {
        for seed in 0..32 {
            let (peaks, lessons) = taught(seed, 400);
            for &((kind, class), _) in &lessons {
                let latest = lessons.iter().rev().find(|it| it.0 == (kind, class)).map(|it| it.1);
                assert_eq!(peaks.peak(kind, class), latest, "seed {seed}");
                assert_eq!(peaks.forecast(kind, class), latest.unwrap_or(0), "seed {seed}");
            }
            assert_eq!(peaks.peak(1, 0), None);
            assert_eq!(peaks.forecast(1, 0), QUANTUM);
        }
        assert_eq!(forecast_of(0), 0);
        assert_eq!(forecast_of(STANDING), 0);
        assert_eq!(forecast_of(STANDING + 1), STANDING + QUANTUM);
        assert_eq!(forecast_of(40 << 30), 40 << 30);
        assert_eq!(class_of(0), 0);
        assert_eq!(class_of(1), 1);
        assert_eq!(class_of(1 << 20), 21);
    }

    /// The invariant (persisted-state-round-trips): a record decoded from its bytes is the
    /// record, and encodes to the same bytes again, byte for byte; two records taught the same
    /// lessons in another order give the same bytes.
    #[test]
    fn peaks_round_trip_byte_for_byte() {
        for seed in 0..32 {
            let (peaks, mut lessons) = taught(seed, 300);
            let bytes = peaks.encode();
            let back = Peaks::decode(&bytes).unwrap_or_else(|why| panic!("seed {seed}: {why}"));
            assert_eq!(back, peaks, "seed {seed}");
            assert_eq!(back.encode(), bytes, "seed {seed}: re-encoded");
            // The same final peaks, taught key by key in reverse order of last teaching.
            let mut again = Peaks::new();
            lessons.reverse();
            let mut seen = Vec::new();
            for (key, peak) in lessons {
                if !seen.contains(&key) {
                    seen.push(key);
                    again.observe(key.0, key.1, peak);
                }
            }
            assert_eq!(again.encode(), bytes, "seed {seed}: another order");
        }
        let empty = Peaks::new().encode();
        assert_eq!(Peaks::decode(&empty), Ok(Peaks::new()));
    }

    /// The invariant: bytes of a previous layout are refused naming both layouts, the per-kind
    /// estimates the image held before (`blzest01`) as layout 0; bytes of no layout, cut short,
    /// with more after or out of order are refused by name.
    #[test]
    fn a_previous_layout_is_refused_naming_both() {
        let mut estimates = b"blzest01".to_vec();
        estimates.extend_from_slice(&[0; 16]);
        let refused = Peaks::decode(&estimates);
        assert_eq!(refused, Err(PeaksError::Layout { found: 0, reads: PEAKS_LAYOUT }));
        let said = refused.unwrap_err().to_string();
        assert!(said.contains("layout 0") && said.contains("layout 1"), "{said}");

        let mut peaks = Peaks::new();
        peaks.observe(0x4000, 3, 1 << 30);
        peaks.observe(0x4000, 4, 2 << 30);
        let bytes = peaks.encode();
        let mut next = bytes.clone();
        next[8..16].copy_from_slice(&(PEAKS_LAYOUT + 1).to_le_bytes());
        assert_eq!(
            Peaks::decode(&next),
            Err(PeaksError::Layout { found: PEAKS_LAYOUT + 1, reads: PEAKS_LAYOUT })
        );
        assert!(matches!(Peaks::decode(b"not peaks at all"), Err(PeaksError::Magic { .. })));
        assert!(matches!(Peaks::decode(&bytes[..bytes.len() - 1]), Err(PeaksError::Length { .. })));
        assert!(matches!(Peaks::decode(&bytes[..4]), Err(PeaksError::Short { .. })));
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(matches!(Peaks::decode(&longer), Err(PeaksError::Length { .. })));
        let mut swapped = bytes.clone();
        let first = 24;
        let (a, b) = swapped[first..].split_at_mut(ENTRY_BYTES);
        a.swap_with_slice(&mut b[..ENTRY_BYTES]);
        assert_eq!(Peaks::decode(&swapped), Err(PeaksError::Order { kind: 0x4000, class: 3 }));
    }

    /// Admission forecasts every node it pushes and records every task that ends: once each
    /// kind and class has been seen, neither allocates.
    #[test]
    fn warm_forecasts_and_observations_allocate_nothing() {
        assert!(blaze_alloc_count::installed(), "the tests count allocations");
        let (mut peaks, lessons) = taught(11, 512);
        blaze_alloc_count::assert_at_most(0, "warm forecasts and observations", || {
            let mut held = 0u64;
            for &((kind, class), peak) in &lessons {
                held = held.wrapping_add(peaks.forecast(kind, class));
                peaks.observe(kind, class, peak / 2);
            }
            std::hint::black_box(held)
        });
    }
}
