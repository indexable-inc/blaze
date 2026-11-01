//! What a task is expected to hold, for the plane's memory admission (docs/v2/plan/lanes/core.md section 4).
//!
//! Every task runs in a bump arena of its own, so what it held at most is measured, not
//! guessed. [`Estimates`] keeps, for each kind of task (a query kind), what tasks of it held,
//! and predicts the next one's peak from it: the [`crate::machine::Spec::memory`] a node is
//! added with. A task over its prediction is let finish and the estimate grows; nothing is
//! cut short or retried. The estimates are kept across runs, in the daemon's image
//! ([`Estimates::encode`]).
//!
//! The rule, integers only, the same observations giving the same estimates: a peak at or over
//! the prediction becomes the prediction at once. A peak under it counts toward a window of
//! [`WINDOW`] of them in a row; once the window is full the prediction comes down half the way
//! (rounded up) to the most of the window's peaks, and a new window begins. So the prediction is a high
//! quantile of what the kind held lately (the most of the last [`WINDOW`] peaks, approached by
//! halves), never under the last peak seen, never over the most ever seen.
//!
//! A task's peak is everything it held: what it allocated through the global allocator
//! (`arena::measure`) and what its frames held (`blaze_mem::frame`). Each kind belongs to a
//! subsystem ([`tag_of`]), and each subsystem has a budget, a share of the machine's
//! ([`set_budgets`]): the scheduler holds back a task of a subsystem over its budget
//! (`blaze_mem::tag::over_budget`) before the machine's budget is reached.

#![forbid(unsafe_code)]

use std::fmt;

use blaze_format::cursor::{self, Put as _, Reader};
use blaze_hash::HashMap;
use blaze_mem::{Tag, tag};
use blaze_radix as radix;
use serde::{Deserialize, Serialize};

/// How many peaks under a prediction, in a row, bring it down a step.
pub const WINDOW: u32 = 64;

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
    /// A program's codegen: of a binary, test, bench, example or `cdylib`. One its predictor
    /// has not measured is predicted the whole budget and runs alone
    /// ([`super::Predict::measured`]), as [`PROGRAM_LINK`] is.
    pub const PROGRAM: TaskKind = TaskKind(0x1_0009);
    /// A unit's names: whether its record is kept, its crate's def map, its check's bodies.
    pub const UNIT_NAMES: TaskKind = TaskKind(0x1_000a);
    /// A unit's check: the diagnostics of every body of its crate.
    pub const UNIT_BODIES: TaskKind = TaskKind(0x1_000b);
    /// A unit's link: its images linked from its codegen, a proc macro's expanders loaded.
    pub const UNIT_LINK: TaskKind = TaskKind(0x1_000c);
    /// A unit's outputs hashed, and its record encoded, for the store.
    pub const UNIT_STORE: TaskKind = TaskKind(0x1_000d);
    /// A program's link, admitted as [`PROGRAM`] is.
    pub const PROGRAM_LINK: TaskKind = TaskKind(0x1_000e);
}

/// The subsystem whose memory a task of `kind` holds: whose budget holds it back.
///
/// Query kinds go by `blaze_query::kinds`' ranges (inputs, facts `0x1000`, the front end
/// `0x2000`, codegen and link `0x4000`, user code `0x5000`, the units' loads `0x6000`), the
/// front end's and the back end's ranges split by subsystem as written here and in
/// docs/v2/plan/lanes/mem/core.md ("Kinds and tags"); the daemon's own by [`kinds`]. A kind
/// none of them names is [`Tag::Other`]'s.
pub const fn tag_of(kind: TaskKind) -> Tag {
    match kind {
        kinds::FETCH | kinds::UNIT_STORE => Tag::Store,
        kinds::LIBRARY | kinds::COMPILE | kinds::REACH | kinds::PROGRAM => Tag::Codegen,
        kinds::UNIT_NAMES => Tag::Nameres,
        kinds::UNIT_BODIES => Tag::Infer,
        kinds::UNIT_LINK | kinds::PROGRAM_LINK => Tag::Link,
        kinds::LOWER | kinds::UB_TESTS | kinds::EVALUATE => Tag::Mir,
        kinds::WALK | TaskKind(4) => Tag::Query,
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

/// Each subsystem's budget as a share of the machine's, in 64ths. The shares sum past 64 on
/// purpose: a build's phases peak at different times (names before bodies, bodies before
/// code, code before the link), so each subsystem may take a large part of the machine while
/// the others are small, and the machine's own budget still holds them all. No subsystem but
/// the frames of every worker together may take more than half, so one that runs away is held
/// back while the others still fit.
pub const fn share(tag: Tag) -> u64 {
    match tag {
        Tag::Infer | Tag::Codegen | Tag::Link | Tag::Frames => 32,
        Tag::Mir | Tag::Query => 24,
        Tag::Nameres | Tag::Intern | Tag::Diagnostics => 16,
        Tag::Parse | Tag::Expand | Tag::Store | Tag::Vfs | Tag::Chunks | Tag::Fuzz => 8,
        Tag::Usercode | Tag::Other => 4,
        Tag::Plane | Tag::Io | Tag::Api => 2,
    }
}

/// Sets each subsystem's budget ([`tag::set_budget`]) to its [`share`] of `machine` bytes.
pub fn set_budgets(machine: u64) {
    for tag in Tag::ALL {
        tag::set_budget(tag, machine / 64 * share(tag));
    }
}

/// What tasks of one kind held: 40 bytes.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Estimate {
    /// How many were measured.
    pub seen: u64,
    /// The most any of them held, in bytes.
    pub most: u64,
    /// A high quantile of what they held, in bytes: what a new task is predicted to hold.
    pub predicted: u64,
    /// How many peaks under `predicted` came in a row since it last moved: the window.
    #[serde(default)]
    pub below: u32,
    /// The most of the window's peaks.
    #[serde(default)]
    pub below_most: u64,
}

const _: () = assert!(size_of::<Estimate>() == 40);

impl Estimate {
    /// One more task held `peak` bytes at most: `predicted` rises to a peak at or above it at
    /// once, and falls halfway to the window's most after [`WINDOW`] peaks below it in a row.
    pub fn observe(&mut self, peak: u64) {
        self.seen = self.seen.saturating_add(1);
        self.most = self.most.max(peak);
        if peak >= self.predicted {
            self.predicted = peak;
            self.below = 0;
            self.below_most = 0;
            return;
        }
        self.below += 1;
        self.below_most = self.below_most.max(peak);
        if self.below >= WINDOW {
            self.predicted -= (self.predicted - self.below_most).div_ceil(2);
            self.below = 0;
            self.below_most = 0;
        }
    }

    /// Whether it is one [`Estimate::observe`] can make.
    fn is_sound(&self) -> bool {
        self.seen > 0
            && self.predicted <= self.most
            && self.below < WINDOW
            && ((self.below == 0 && self.below_most == 0)
                || (u64::from(self.below) <= self.seen && self.below_most < self.predicted))
    }
}

/// Each kind's [`Estimate`], learned as tasks finish. A predict or an observe is one hash of
/// the kind's `u32` and one probe; an observe of a kind seen before allocates nothing. Kinds
/// are written in order of their numbers, by [`Estimates::encode`] and by serde alike.
#[derive(Serialize, Deserialize, Debug, Default, PartialEq, Eq)]
pub struct Estimates {
    #[serde(with = "in_order")]
    pub by_kind: HashMap<TaskKind, Estimate>,
    /// What a task of a kind never measured is predicted to hold.
    pub unseen: u64,
}

/// [`Estimates::by_kind`] for serde, its kinds in order, so the same estimates give the same
/// bytes whatever order their kinds were first seen in.
mod in_order {
    use blaze_hash::HashMap;
    use serde::{Deserialize, Deserializer, Serializer};

    use super::{Estimate, TaskKind};

    pub(super) fn serialize<S: Serializer>(
        by_kind: &HashMap<TaskKind, Estimate>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_map(super::sorted(by_kind))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<HashMap<TaskKind, Estimate>, D::Error> {
        HashMap::deserialize(deserializer)
    }
}

/// The kinds of `by_kind` and their estimates, in order of the kinds' numbers.
fn sorted(by_kind: &HashMap<TaskKind, Estimate>) -> Vec<(TaskKind, Estimate)> {
    // alloc: once per encoding of the estimates, as the daemon's image is written
    let mut kinds: Vec<(TaskKind, Estimate)> = by_kind.iter().map(|(&k, &e)| (k, e)).collect();
    radix::sort_copy_by_key(&mut kinds, |it| it.0.0);
    kinds
}

/// The first bytes of [`Estimates::encode`]'s bytes, its version with them.
const MAGIC: &[u8; 8] = b"blzest01";
/// The bytes of one kind in [`Estimates::encode`]'s bytes.
const KIND_BYTES: usize = 4 + 4 + 8 * 4;

impl Estimates {
    /// No kind measured yet, each predicted to hold `unseen`.
    pub fn new(unseen: u64) -> Estimates {
        Estimates { by_kind: HashMap::default(), unseen }
    }

    /// The peak a task of `kind` is predicted to hold, in bytes.
    pub fn predict(&self, kind: TaskKind) -> u64 {
        match self.by_kind.get(&kind) {
            Some(estimate) if estimate.seen > 0 => estimate.predicted,
            _ => self.unseen,
        }
    }

    /// A task of `kind` finished having held `peak` bytes at most.
    pub fn observe(&mut self, kind: TaskKind, peak: u64) {
        self.by_kind.entry(kind).or_default().observe(peak);
    }

    /// The estimates as bytes for the daemon's image: the same estimates give the same bytes.
    /// Integers are little-endian: `MAGIC`, `unseen`, the count of kinds, then each kind in
    /// order: its number, `below`, `seen`, `most`, `predicted`, `below_most`.
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(MAGIC.len() + 16 + self.by_kind.len() * KIND_BYTES);
        bytes.put_bytes(MAGIC);
        bytes.put_u64(self.unseen);
        bytes.put_u64(self.by_kind.len() as u64);
        for (kind, estimate) in sorted(&self.by_kind) {
            bytes.put_u32(kind.0);
            bytes.put_u32(estimate.below);
            for it in [estimate.seen, estimate.most, estimate.predicted, estimate.below_most] {
                bytes.put_u64(it);
            }
        }
        bytes
    }

    /// The estimates [`Estimates::encode`] gave `bytes`, or what is wrong with them: bytes of
    /// another version, cut short or with more after, kinds out of order, or an estimate no
    /// run of observations makes.
    pub fn decode(bytes: &[u8]) -> Result<Estimates, DecodeError> {
        let mut read = Reader::new(bytes);
        if read.array::<8>()? != MAGIC {
            return Err(DecodeError::Version);
        }
        let unseen = read.u64()?;
        let count = read.u64()?;
        if count.checked_mul(KIND_BYTES as u64) != Some(read.left() as u64) {
            return Err(DecodeError::Length { count, bytes: bytes.len() });
        }
        let mut by_kind = HashMap::default();
        by_kind.reserve(count as usize);
        let mut last: Option<u32> = None;
        for _ in 0..count {
            let kind = read.u32()?;
            if last.is_some_and(|it| it >= kind) {
                return Err(DecodeError::Order { kind });
            }
            last = Some(kind);
            let estimate = Estimate {
                below: read.u32()?,
                seen: read.u64()?,
                most: read.u64()?,
                predicted: read.u64()?,
                below_most: read.u64()?,
            };
            if !estimate.is_sound() {
                return Err(DecodeError::Unsound { kind });
            }
            by_kind.insert(TaskKind(kind), estimate);
        }
        Ok(Estimates { by_kind, unseen })
    }
}

/// Why bytes are not [`Estimates`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// Not of this version of [`Estimates::encode`].
    Version,
    Short { at: usize },
    /// Not as many bytes as `count` kinds take.
    Length { count: u64, bytes: usize },
    /// A kind not after the one before it.
    Order { kind: u32 },
    /// An estimate no observations make.
    Unsound { kind: u32 },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Version => write!(f, "memory estimates of another version"),
            DecodeError::Short { at } => write!(f, "memory estimates cut short at byte {at}"),
            DecodeError::Length { count, bytes } => {
                write!(f, "memory estimates of {count} kinds in {bytes} bytes")
            }
            DecodeError::Order { kind } => {
                write!(f, "memory estimates with kind {kind} out of order")
            }
            DecodeError::Unsound { kind } => {
                write!(f, "memory estimate of kind {kind} that no observations make")
            }
        }
    }
}

impl std::error::Error for DecodeError {}

impl From<cursor::Error> for DecodeError {
    fn from(error: cursor::Error) -> DecodeError {
        DecodeError::Short { at: error.at }
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
    /// What a task of `kind` with `key` is predicted to cost.
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

/// Today's predictor: peaks by kind, costs left as the specs give them.
impl Predict for Estimates {
    fn predict(&self, kind: TaskKind, _key: Option<crate::Key>) -> Prediction {
        Prediction { cost: None, peak: Estimates::predict(self, kind) }
    }

    fn observe(&mut self, kind: TaskKind, _key: Option<crate::Key>, _ran_ns: u64, peak: u64) {
        Estimates::observe(self, kind, peak);
    }

    fn encode(&self) -> Vec<u8> {
        Estimates::encode(self)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use blaze_ids::rng::SplitMix64;

    use super::*;

    /// A peak of one of a few sizes, now and then far bigger.
    fn peak(random: &mut SplitMix64) -> u64 {
        match random.below(20) {
            0 => random.below(1 << 34),
            1..=3 => 0,
            _ => (1 + random.below(8)) << (10 + random.below(16)),
        }
    }

    #[test]
    fn estimates_predict_peaks_and_keep_the_specs_costs() {
        // Through the trait, as the scheduler asks: the inherent methods take no key.
        let mut predict = Estimates::new(64);
        let key = Some(crate::Key([1; 32]));
        let peak = Predict::predict(&predict, TaskKind(2), key);
        assert_eq!(peak, Prediction { cost: None, peak: 64 });
        Predict::observe(&mut predict, TaskKind(2), key, 5_000, 900);
        let prediction = Predict::predict(&predict, TaskKind(2), None);
        assert_eq!(prediction, Prediction { cost: None, peak: 900 });
        let mut estimates = Estimates::new(64);
        estimates.observe(TaskKind(2), 900);
        assert_eq!(Predict::encode(&predict), estimates.encode());

        let mut spec = crate::Spec {
            id: crate::NodeId(0),
            kind: blaze_protocol::events::NodeKind::Codegen,
            name: "f".into(),
            deps: Vec::new(),
            pool: crate::Pool::Work,
            cost: 3,
            key,
            memory: 1000,
        };
        prediction.apply(&mut spec);
        assert_eq!((spec.cost, spec.memory), (3, 1000));
        Prediction { cost: Some(7), peak: 2000 }.apply(&mut spec);
        assert_eq!((spec.cost, spec.memory), (7, 2000));
    }

    #[test]
    fn a_kind_never_measured_is_unseen() {
        let mut estimates = Estimates::new(7 << 20);
        assert_eq!(estimates.predict(TaskKind(3)), 7 << 20);
        estimates.observe(TaskKind(3), 100);
        assert_eq!(estimates.predict(TaskKind(3)), 100);
        assert_eq!(estimates.predict(TaskKind(4)), 7 << 20);
    }

    #[test]
    fn a_peak_over_the_prediction_is_the_prediction_at_once() {
        let mut estimates = Estimates::new(0);
        let kind = TaskKind(1);
        estimates.observe(kind, 1000);
        estimates.observe(kind, 10);
        assert_eq!(estimates.predict(kind), 1000);
        estimates.observe(kind, 5000);
        assert_eq!(estimates.predict(kind), 5000);
        let estimate = estimates.by_kind[&kind];
        assert_eq!((estimate.seen, estimate.most, estimate.below), (3, 5000, 0));
    }

    #[test]
    fn smaller_peaks_bring_it_down_by_halves_a_window_at_a_time() {
        let mut estimates = Estimates::new(0);
        let kind = TaskKind(1);
        estimates.observe(kind, 1 << 20);
        for _ in 0..WINDOW - 1 {
            estimates.observe(kind, 1024);
            assert_eq!(estimates.predict(kind), 1 << 20);
        }
        estimates.observe(kind, 1024);
        assert_eq!(estimates.predict(kind), 1024 + ((1 << 20) - 1024) / 2);
        let mut windows = 1;
        while estimates.predict(kind) > 1024 {
            for _ in 0..WINDOW {
                estimates.observe(kind, 1024);
            }
            windows += 1;
        }
        assert_eq!(estimates.predict(kind), 1024);
        assert!(windows <= 21, "{windows} windows");
        assert_eq!(estimates.by_kind[&kind].most, 1 << 20);
    }

    /// Over random runs of peaks of many kinds, after every observation: the prediction is not
    /// under the peak just seen nor over the most seen, it comes down only at the end of a full
    /// window of smaller peaks and never under the most of them, and it went up only to a peak.
    #[test]
    fn random_peaks_keep_the_rule() {
        for seed in 0..200u64 {
            let mut random = SplitMix64::new(seed);
            let mut estimates = Estimates::new(random.below(1 << 30));
            let mut smaller_in_a_row = Windows::new();
            for _ in 0..2000 {
                let kind = TaskKind(random.below(4) as u32);
                let peak = peak(&mut random);
                let before = estimates.by_kind.get(&kind).copied();
                estimates.observe(kind, peak);
                let after = estimates.by_kind[&kind];
                let predicted = estimates.predict(kind);
                assert_eq!(predicted, after.predicted);
                assert!(predicted >= peak, "seed {seed}: {predicted} under the last peak {peak}");
                assert!(predicted <= after.most, "seed {seed}: {predicted} over {}", after.most);
                assert!(after.is_sound(), "seed {seed}: {after:?}");
                let window = smaller_in_a_row.entry(kind).or_default();
                match before {
                    Some(before) if peak < before.predicted => {
                        window.0 += 1;
                        window.1 = window.1.max(peak);
                        if predicted < before.predicted {
                            assert_eq!(window.0, WINDOW, "seed {seed}: down after {}", window.0);
                            assert!(predicted >= window.1, "seed {seed}");
                            *window = (0, 0);
                        } else {
                            assert_eq!(predicted, before.predicted, "seed {seed}");
                        }
                    }
                    _ => {
                        assert_eq!(predicted, peak, "seed {seed}");
                        *window = (0, 0);
                    }
                }
            }
        }
    }

    /// Per kind: the smaller peaks in a row since its prediction moved, and the most of them.
    type Windows = BTreeMap<TaskKind, (u32, u64)>;

    #[test]
    fn the_same_observations_give_the_same_estimates_and_bytes() {
        for seed in 0..50u64 {
            let run = || {
                let mut random = SplitMix64::new(seed);
                let mut estimates = Estimates::new(1 << 20);
                for _ in 0..1000 {
                    let kind = TaskKind(random.below(6) as u32);
                    estimates.observe(kind, peak(&mut random));
                }
                estimates
            };
            let (a, b) = (run(), run());
            assert_eq!(a, b, "seed {seed}");
            assert_eq!(a.encode(), b.encode(), "seed {seed}");
        }
    }

    /// The bytes [`Estimates::encode`] gave while the kinds were a `BTreeMap`: kinds in order.
    fn encoded(model: &BTreeMap<TaskKind, Estimate>, unseen: u64) -> Vec<u8> {
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&unseen.to_le_bytes());
        bytes.extend_from_slice(&(model.len() as u64).to_le_bytes());
        for (kind, estimate) in model {
            bytes.extend_from_slice(&kind.0.to_le_bytes());
            bytes.extend_from_slice(&estimate.below.to_le_bytes());
            for it in [estimate.seen, estimate.most, estimate.predicted, estimate.below_most] {
                bytes.extend_from_slice(&it.to_le_bytes());
            }
        }
        bytes
    }

    /// The hashed table predicts what the ordered map it replaced predicted, after every
    /// observation of kinds far apart (query kinds, the daemon's own, `u32::MAX`), and its
    /// bytes are the map's.
    #[test]
    fn the_hashed_table_says_what_the_ordered_map_said() {
        let kinds = [0, 1, 7, 300, 0x6000, 0x1_0001, 0x1_0004, u32::MAX];
        for seed in 0..100u64 {
            let mut random = SplitMix64::new(seed);
            let unseen = random.below(1 << 24);
            let mut estimates = Estimates::new(unseen);
            let mut model: BTreeMap<TaskKind, Estimate> = BTreeMap::new();
            for _ in 0..1500 {
                let kind = TaskKind(kinds[random.index(kinds.len())]);
                let peak = peak(&mut random);
                estimates.observe(kind, peak);
                model.entry(kind).or_default().observe(peak);
                let asked = TaskKind(kinds[random.index(kinds.len())]);
                let want = model.get(&asked).filter(|it| it.seen > 0).map_or(unseen, |it| it.predicted);
                assert_eq!(estimates.predict(asked), want, "seed {seed}");
            }
            assert_eq!(estimates.by_kind.len(), model.len(), "seed {seed}");
            assert_eq!(estimates.encode(), encoded(&model, unseen), "seed {seed}");
        }
    }

    /// The same estimates, their kinds first seen in another order, give the same bytes, by
    /// [`Estimates::encode`] and by serde, and come back from both.
    #[test]
    fn the_order_kinds_came_in_changes_no_byte() {
        for seed in 0..50u64 {
            let mut random = SplitMix64::new(seed);
            let runs: Vec<(TaskKind, Vec<u64>)> = (0..1 + random.below(40))
                .map(|kind| {
                    let kind = TaskKind((kind as u32).wrapping_mul(0x9e37_79b9));
                    (kind, (0..1 + random.below(200)).map(|_| peak(&mut random)).collect())
                })
                .collect();
            let mut forward = Estimates::new(5);
            for (kind, peaks) in &runs {
                for &peak in peaks {
                    forward.observe(*kind, peak);
                }
            }
            let mut backward = Estimates::new(5);
            for (kind, peaks) in runs.iter().rev() {
                for &peak in peaks {
                    backward.observe(*kind, peak);
                }
            }
            assert_eq!(forward, backward, "seed {seed}");
            assert_eq!(forward.encode(), backward.encode(), "seed {seed}");
            let json = serde_json::to_vec(&forward).unwrap();
            assert_eq!(json, serde_json::to_vec(&backward).unwrap(), "seed {seed}");
            assert_eq!(serde_json::from_slice::<Estimates>(&json).unwrap(), forward, "seed {seed}");
            assert_eq!(Estimates::decode(&forward.encode()), Ok(forward), "seed {seed}");
        }
    }

    #[test]
    fn estimates_come_back_from_their_bytes_and_bad_bytes_are_refused() {
        let mut random = SplitMix64::new(9);
        let mut estimates = Estimates::new(123);
        for _ in 0..500 {
            estimates.observe(TaskKind(random.below(5) as u32), peak(&mut random));
        }
        let bytes = estimates.encode();
        assert_eq!(Estimates::decode(&bytes).as_ref(), Ok(&estimates));
        assert_eq!(Estimates::decode(&Estimates::new(5).encode()), Ok(Estimates::new(5)));
        assert_eq!(Estimates::decode(b"blzest00"), Err(DecodeError::Version));
        assert_eq!(Estimates::decode(&bytes[..12]), Err(DecodeError::Short { at: 8 }));
        assert!(matches!(
            Estimates::decode(&bytes[..bytes.len() - 1]),
            Err(DecodeError::Length { .. })
        ));
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(matches!(Estimates::decode(&longer), Err(DecodeError::Length { .. })));
        // The predicted bytes of the first kind made more than the most.
        let mut unsound = bytes.clone();
        let first = MAGIC.len() + 16;
        unsound[first + 24..first + 32].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(matches!(Estimates::decode(&unsound), Err(DecodeError::Unsound { .. })));
        // The second kind's number made the first's.
        let mut order = bytes;
        let kind = order[first..first + 4].to_vec();
        order[first + KIND_BYTES..first + KIND_BYTES + 4].copy_from_slice(&kind);
        assert!(matches!(Estimates::decode(&order), Err(DecodeError::Order { .. })));
    }

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

    /// Every subsystem has a share; none but the workers' frames together takes more than
    /// half the machine, and every budget is its share of the machine's.
    #[test]
    fn each_subsystem_gets_its_share_of_the_machine() {
        for tag in Tag::ALL {
            assert!((1..=32).contains(&share(tag)), "{tag}");
        }
        let machine = 64 << 30;
        set_budgets(machine);
        for tag in Tag::ALL {
            assert_eq!(tag::budget(tag), machine / 64 * share(tag), "{tag}");
            assert!(tag::budget(tag) <= machine / 2, "{tag}");
        }
        assert_eq!(tag::budget(Tag::Infer), 32 << 30);
        assert_eq!(tag::budget(Tag::Plane), 2 << 30);
    }

    /// The plane predicts every node it admits and observes every task that ends: once each
    /// kind has been seen, neither allocates.
    #[test]
    fn warm_predictions_and_observations_allocate_nothing() {
        assert!(blaze_alloc_count::installed(), "the tests count allocations");
        let mut estimates = Estimates::new(64);
        let kinds: Vec<TaskKind> = (0..32).map(TaskKind).collect();
        let mut random = SplitMix64::new(11);
        for &kind in &kinds {
            estimates.observe(kind, peak(&mut random));
        }
        let peaks: Vec<u64> = (0..1024).map(|_| peak(&mut random)).collect();
        blaze_alloc_count::assert_at_most(0, "warm predictions and observations", || {
            let mut held = 0u64;
            for (at, &most) in peaks.iter().enumerate() {
                let kind = kinds[at % kinds.len()];
                held = held.wrapping_add(Predict::predict(&estimates, kind, None).peak);
                Predict::observe(&mut estimates, kind, None, 0, most);
            }
            std::hint::black_box(held)
        });
    }
}
