//! Each kind's run time: the prediction for a task whose key has no record, or has none.
//! Kinds are few and kept (never evicted); their peaks are the plane's own
//! [`blaze_graph::admission::Estimates`], learned beside these.

use blaze_graph::admission::TaskKind;
use blaze_hash::HashMap;
use blaze_ids::radix;

/// A new run's weight against the mean, as a shift: an eighth.
const SHIFT: u32 = 3;

/// The run times of one kind's tasks: 16 bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Time {
    pub seen: u64,
    /// A mean of its runs, in nanoseconds, each new run moving it an eighth of the way, in
    /// integers: never under the least run seen nor over the most.
    pub mean_ns: u64,
}

const _: () = assert!(size_of::<Time>() == 16);

impl Time {
    fn observe(&mut self, ran_ns: u64) {
        self.mean_ns = if self.seen == 0 {
            ran_ns
        } else if ran_ns >= self.mean_ns {
            self.mean_ns + (ran_ns - self.mean_ns).div_ceil(1 << SHIFT)
        } else {
            self.mean_ns - (self.mean_ns - ran_ns) / (1 << SHIFT)
        };
        self.seen = self.seen.saturating_add(1);
    }
}

/// Each kind's [`Time`]: a predict or an observe is one hash of the kind's `u32` and one
/// probe; an observe of a kind seen before allocates nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Times {
    by_kind: HashMap<TaskKind, Time>,
}

impl Times {
    /// The run times of `kind`, once one ran.
    pub fn get(&self, kind: TaskKind) -> Option<Time> {
        self.by_kind.get(&kind).copied().filter(|it| it.seen > 0)
    }

    pub fn observe(&mut self, kind: TaskKind, ran_ns: u64) {
        self.by_kind.entry(kind).or_default().observe(ran_ns);
    }

    pub fn len(&self) -> usize {
        self.by_kind.len()
    }

    /// Each kind and its times, in order of the kinds' numbers.
    pub fn sorted(&self) -> Vec<(TaskKind, Time)> {
        let mut kinds: Vec<(TaskKind, Time)> = self.by_kind.iter().map(|(&k, &t)| (k, t)).collect(); // alloc: once per image written, a few hundred kinds
        radix::sort_copy_by_key(&mut kinds, |it| it.0.0);
        kinds
    }

    /// Times read back, in order of their kinds, each one some observations make.
    pub(crate) fn from_sorted(kinds: Vec<(TaskKind, Time)>) -> Times {
        let mut by_kind = HashMap::default();
        by_kind.reserve(kinds.len());
        by_kind.extend(kinds);
        Times { by_kind }
    }
}
