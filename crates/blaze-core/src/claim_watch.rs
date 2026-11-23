//! The watchdog over claims (docs/v2/plan/design/parallel.md, "End state (architect,
//! 2026-11-16)", point 3, and its Pieces item 13; decision D79): a claim held past its kind's
//! deadline is named as [`Hung`] (rule 18: a hang is a typed failure, never waited out).
//!
//! - **What it watches.** The driver tells it each claim it took from the
//!   [`ClaimTable`](crate::claim_table::ClaimTable) ([`ClaimWatch::claimed`]: the slot, its owner,
//!   the record's kind, the clock then) and each publish ([`ClaimWatch::published`]). It holds
//!   only the claims in flight, never the table.
//! - **When it names one.** [`ClaimWatch::tick`] is given the clock now and each kind's deadline;
//!   a claim held at least its kind's deadline is pushed as a [`Hung`], each claim ONCE in its
//!   life, the tick's findings sorted by (kind, slot) so two runs of one schedule print the same
//!   lines (rule 25). What the driver does with a `Hung` (fail the owner's job with a typed
//!   error naming the item) is the driver's; the watch only names it.
//! - **Bounded** (rule 18): at most `cap` claims in flight, given at [`ClaimWatch::new_in`]; one
//!   past it is [`WatchError::Full`], never a grow. All room is taken at `new_in`; a tick takes
//!   room in the caller's `out` once, before it marks anything, so a refused tick changes
//!   nothing.
//! - **Cost.** A claim and a publish are O(1) (an open-addressed index by slot, at most half
//!   full, deletion by backward shift); a tick is one pass over the claims in flight, which are
//!   at most the live tasks (each owns at most the one claim it runs): the floor for a watch
//!   that reads every claim's age.
//!
//! Sans-IO: no clock (the caller passes `now`, so a simulation's virtual clock drives it), no
//! thread, no allocation after `new_in`. Its tests pass (green b13ac2dcb9,
//! 2026-11-17).

#![blaze::sans_io]

use core::{alloc::Allocator, fmt};

use blaze_own::Vec;

use crate::claim_table::{At, TaskId};

/// Nanoseconds on the clock the driver passes in (a simulation's virtual clock, or a
/// `wait::Monotonic` counted from the command's start). Only differences are read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Nanos(pub u64);

/// A claim held past its kind's deadline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hung {
    /// The claim table's slot.
    pub at: At,
    /// The task that owns it and has not published.
    pub by: TaskId,
    /// The record's kind, as the driver numbers kinds.
    pub kind: u16,
    /// How long it had been held at the tick that named it.
    pub held: Nanos,
}

/// A broken bug-seeding plan for the tests' controls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchPlant {
    None,
    /// A publish forgets, besides its own claim, every other claim of the same kind: the
    /// stuck owner of that kind is never named.
    ForgetOnPublishOfAnother,
}

/// Why the watch refused (rule 19: each names the slot or the bound).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchError {
    /// `cap` claims are in flight already.
    Full { cap: u32, at: At },
    /// The slot is watched already: the claim table claims a slot once in its life.
    AlreadyWatched { at: At, by: TaskId },
    /// A publish of a slot the watch was never told was claimed.
    NotWatched { at: At },
    /// A tick's clock is before a claim's start.
    ClockBack { at: At, since: Nanos, now: Nanos },
    /// The watch's room (at `new_in`) or the tick's `out` room could not be had.
    Memory { wanted: u32 },
}

impl fmt::Display for WatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            WatchError::Full { cap, at } => write!(f, "claim slot {}: the claim watch already holds its {cap} claims in flight (rule 18: a table has a cap)", at.0),
            WatchError::AlreadyWatched { at, by } => write!(f, "claim slot {}: told claimed by task {} while already watched (a slot is claimed once; rule 3)", at.0, by.0),
            WatchError::NotWatched { at } => write!(f, "claim slot {}: published but never told claimed (rule 3)", at.0),
            WatchError::ClockBack { at, since, now } => write!(f, "claim slot {}: the tick's clock {} ns is before the claim's start {} ns (rule 3)", at.0, now.0, since.0),
            WatchError::Memory { wanted } => write!(f, "the claim watch could not have room for {wanted} entries"),
        }
    }
}

impl core::error::Error for WatchError {}

/// A claim in flight.
#[derive(Clone, Copy, Debug)]
struct Watched {
    at: At,
    by: TaskId,
    kind: u16,
    since: Nanos,
    /// Named as hung by a tick already.
    named: bool,
}

/// An index bucket holding no claim.
const NO_ENTRY: u32 = u32::MAX;

/// The watch. See the module's doc.
pub struct ClaimWatch<A: Allocator> {
    /// The claims in flight, in no order.
    live: Vec<Watched, A>,
    /// Open-addressed by slot, linear probing: each bucket `NO_ENTRY` or a position in `live`.
    /// Its length is a power of two at least twice `cap`, so a probe meets an empty bucket.
    index: Vec<u32, A>,
    cap: u32,
    plant: WatchPlant,
}

impl<A: Allocator> ClaimWatch<A> {
    /// A watch of at most `cap` claims in flight, its room taken from `alloc` now and never
    /// again.
    pub fn new_in(cap: u32, alloc: A) -> Result<ClaimWatch<A>, WatchError>
    where
        A: Copy,
    {
        Self::planted_in(cap, WatchPlant::None, alloc)
    }

    /// [`ClaimWatch::new_in`] with a plant (the tests' controls).
    pub fn planted_in(cap: u32, plant: WatchPlant, alloc: A) -> Result<ClaimWatch<A>, WatchError>
    where
        A: Copy,
    {
        let refused = WatchError::Memory { wanted: cap };
        let buckets = cap.max(1).checked_mul(2).and_then(u32::checked_next_power_of_two).ok_or(refused)?;
        let live = Vec::try_with_capacity_in(cap as usize, alloc).map_err(|_| refused)?;
        let mut index = Vec::try_with_capacity_in(buckets as usize, alloc).map_err(|_| refused)?;
        for _ in 0..buckets {
            index.try_push(NO_ENTRY).map_err(|_| refused)?;
        }
        Ok(ClaimWatch { live, index, cap, plant })
    }

    /// The claims in flight.
    pub fn live(&self) -> u32 {
        self.live.len() as u32
    }

    /// Task `by` claimed slot `at`, for a record of `kind`, at `now`.
    pub fn claimed(&mut self, at: At, by: TaskId, kind: u16, now: Nanos) -> Result<(), WatchError> {
        let bucket = match self.find(at) {
            Found::At(_) => return Err(WatchError::AlreadyWatched { at, by }),
            Found::Empty(bucket) => bucket,
        };
        if self.live.len() as u32 >= self.cap {
            return Err(WatchError::Full { cap: self.cap, at });
        }
        let position = self.live.len() as u32;
        self.live
            .try_push(Watched { at, by, kind, since: now, named: false })
            .map_err(|_| WatchError::Memory { wanted: self.cap })?;
        self.index[bucket] = position;
        Ok(())
    }

    /// Slot `at` was published: its claim leaves the watch.
    pub fn published(&mut self, at: At) -> Result<(), WatchError> {
        let Found::At(bucket) = self.find(at) else {
            return Err(WatchError::NotWatched { at });
        };
        let kind = self.live[self.index[bucket] as usize].kind;
        self.remove(bucket);
        if self.plant == WatchPlant::ForgetOnPublishOfAnother {
            // The planted bug: every other claim of the kind goes too. Bounded: each round
            // removes one claim, and there are at most `cap`.
            for _ in 0..self.cap {
                let Some(other) = self.live.iter().position(|w| w.kind == kind) else { break };
                let Found::At(b) = self.find(self.live[other].at) else { break };
                self.remove(b);
            }
        }
        Ok(())
    }

    /// Names each claim held at least its kind's deadline at `now` and not named before,
    /// appended to `out` sorted by (kind, slot). A refused tick (the clock before a claim's
    /// start, or no room in `out`) pushes and marks nothing.
    pub fn tick<B: Allocator>(
        &mut self,
        now: Nanos,
        deadline_of: impl Fn(u16) -> Nanos,
        out: &mut Vec<Hung, B>,
    ) -> Result<(), WatchError> {
        // First pass: the clock is checked against every claim and the findings are counted,
        // so the room is taken before anything is marked.
        let mut count: u32 = 0;
        for w in self.live.iter() {
            if now < w.since {
                return Err(WatchError::ClockBack { at: w.at, since: w.since, now });
            }
            if !w.named && now.0 - w.since.0 >= deadline_of(w.kind).0 {
                count += 1;
            }
        }
        if count == 0 {
            return Ok(());
        }
        out.try_reserve(count as usize).map_err(|_| WatchError::Memory { wanted: count })?;
        let start = out.len();
        for w in self.live.iter_mut() {
            let held = Nanos(now.0 - w.since.0);
            if !w.named && held.0 >= deadline_of(w.kind).0 {
                w.named = true;
                out.try_push(Hung { at: w.at, by: w.by, kind: w.kind, held })
                    .map_err(|_| WatchError::Memory { wanted: count })?;
            }
        }
        out[start..].sort_unstable_by_key(|h| (h.kind, h.at.0));
        Ok(())
    }

    /// The bucket a slot's probe starts at.
    fn home(&self, at: At) -> usize {
        let mask = self.index.len() - 1;
        (at.0.wrapping_mul(0x9E37_79B9) as usize) & mask
    }

    /// The bucket holding `at`, or the empty bucket its probe ends at. Bounded: the index is at
    /// most half full, so a probe meets an empty bucket within its length.
    fn find(&self, at: At) -> Found {
        let mask = self.index.len() - 1;
        let mut bucket = self.home(at);
        for _ in 0..self.index.len() {
            let position = self.index[bucket];
            if position == NO_ENTRY {
                return Found::Empty(bucket);
            }
            if self.live[position as usize].at == at {
                return Found::At(bucket);
            }
            bucket = (bucket + 1) & mask;
        }
        unreachable!("the claim watch's index is at most half full, so a probe meets an empty bucket")
    }

    /// Removes the claim at `bucket` from the index (backward shift) and from `live` (swap with
    /// the last, whose bucket is then pointed at its new position).
    fn remove(&mut self, bucket: usize) {
        let position = self.index[bucket] as usize;
        let mask = self.index.len() - 1;
        // Backward shift: each later entry of the probe run that may sit in the hole moves in.
        let mut hole = bucket;
        let mut next = (hole + 1) & mask;
        for _ in 0..self.index.len() {
            let moving = self.index[next];
            if moving == NO_ENTRY {
                break;
            }
            let home = self.home(self.live[moving as usize].at);
            // The entry may move back into the hole when the hole lies on its probe path, from
            // its home up to where it sits.
            if (next.wrapping_sub(home) & mask) >= (next.wrapping_sub(hole) & mask) {
                self.index[hole] = moving;
                hole = next;
            }
            next = (next + 1) & mask;
        }
        self.index[hole] = NO_ENTRY;
        self.live.remove_swap(position);
        if position < self.live.len() {
            // The last claim moved into `position`: point its bucket there.
            let moved = self.live[position].at;
            let old = self.live.len() as u32;
            let mut b = self.home(moved);
            for _ in 0..self.index.len() {
                if self.index[b] == old {
                    self.index[b] = position as u32;
                    break;
                }
                b = (b + 1) & mask;
            }
        }
    }
}

/// What a probe found.
enum Found {
    At(usize),
    Empty(usize),
}

#[cfg(all(test, not(blaze_check)))]
mod tests {
    //! Run: `cargo test -p blaze-core claim_watch` (or blaze's). Every test prints nothing on a
    //! pass; cargo reports `test claim_watch::tests::<name> ... ok` for each of the three.

    use std::alloc::Global;

    use blaze_own::Vec;
    use blaze_sim::{Broke, Config, check};

    use super::{ClaimWatch, Hung, Nanos, WatchError, WatchPlant};
    use crate::claim_table::{At, TaskId};

    const MS: u64 = 1_000_000;

    /// Kind 0's deadline is 50 ms, kind 1's 20 ms.
    fn deadline(kind: u16) -> Nanos {
        match kind {
            0 => Nanos(50 * MS),
            _ => Nanos(20 * MS),
        }
    }

    /// The planner's scenario: two owners of kind 0 claim at 0; A publishes at 5 ms, B never
    /// does. Returns what the ticks at 49, 50 and 60 ms named.
    fn two_owners(plant: WatchPlant) -> [std::vec::Vec<Hung>; 3] {
        let mut watch = ClaimWatch::planted_in(4, plant, Global).expect("a small watch");
        let (a, b) = (At(3), At(11));
        watch.claimed(a, TaskId(0), 0, Nanos(0)).expect("A claims");
        watch.claimed(b, TaskId(1), 0, Nanos(0)).expect("B claims");
        watch.published(a).expect("A publishes at 5 ms");
        [49 * MS, 50 * MS, 60 * MS].map(|now| {
            let mut out: Vec<Hung, Global> = Vec::new_in(Global);
            watch.tick(Nanos(now), deadline, &mut out).expect("a tick");
            out.as_slice().to_vec()
        })
    }

    /// "claim_watch: 1 hung at 50 ms; plant caught" (the scenario half): nothing at 49 ms,
    /// exactly B at the 50 ms deadline with 50 ms held, nothing again at 60 ms (named once).
    #[test]
    fn one_stuck_owner_is_named_once_at_its_deadline() {
        let [before, at, after] = two_owners(WatchPlant::None);
        assert!(before.is_empty(), "nothing is hung at 49 ms: {before:?}");
        assert_eq!(at, [Hung { at: At(11), by: TaskId(1), kind: 0, held: Nanos(50 * MS) }]);
        assert!(after.is_empty(), "a claim is named once: {after:?}");
    }

    /// Control ("plant caught"): a publish that forgets the other claim of its kind names no
    /// one at 50 ms, which the scenario's assertion refuses.
    #[test]
    fn the_oracle_catches_forget_on_publish_of_another() {
        let [_, at, _] = two_owners(WatchPlant::ForgetOnPublishOfAnother);
        assert!(at.is_empty(), "the plant hides the stuck owner: {at:?}");
        assert_ne!(at, two_owners(WatchPlant::None)[1]);
    }

    /// The invariant: on every drawn schedule, hung claims reported by `ClaimWatch` match the reference model (held at least the deadline, unpublished, and named once).
    /// Drawn schedules on the virtual clock: up to 6 owners of two kinds, each claiming at a
    /// drawn millisecond and publishing later or never, ticks at drawn milliseconds; a model
    /// says which claims each tick must name (held at least the deadline, unpublished, not
    /// named before), sorted by (kind, slot); the watch must name exactly those. Also: one
    /// claim past `cap` is `Full`, a publish of an unknown slot is `NotWatched`.
    #[test]
    fn hung_claims_match_the_model_on_drawn_schedules() {
        let config = Config { must_reach: &["a claim was named hung", "a claim was published in time"], ..Config::cases(2_000) };
        check(config, |tape| {
            let owners = tape.int_in(1..=6u32, "owners");
            let cap = tape.int_in(owners..=owners + 2, "cap");
            // (slot, kind, claim ms, publish ms)
            let mut plan: std::vec::Vec<(At, u16, u64, Option<u64>)> = std::vec::Vec::new();
            for i in 0..owners {
                let kind = tape.int_in(0..=1u16, "kind");
                let claim = tape.int_in(0..=40u64, "claim ms");
                let publish = match tape.coin("publishes") {
                    true => Some(claim + tape.int_in(0..=80u64, "held ms")),
                    false => None,
                };
                // Slots collide in the index on purpose: multiples of the index's length.
                plan.push((At(i * 16 + tape.int_in(0..=1u32, "slot step")), kind, claim, publish));
            }
            let mut watch = ClaimWatch::new_in(cap, Global).map_err(|e| Broke::Harness(format!("{e}")))?;
            let mut named = std::vec![false; plan.len()];
            for ms in 0..=130u64 {
                for (i, &(at, kind, claim, _)) in plan.iter().enumerate() {
                    if claim == ms {
                        watch.claimed(at, TaskId(i as u32), kind, Nanos(ms * MS)).map_err(|e| Broke::Oracle(format!("claim: {e}")))?;
                    }
                }
                for &(at, kind, claim, publish) in plan.iter() {
                    if publish == Some(ms) {
                        if ms - claim < deadline(kind).0 / MS {
                            tape.reached("a claim was published in time");
                        }
                        watch.published(at).map_err(|e| Broke::Oracle(format!("publish: {e}")))?;
                    }
                }
                if !tape.coin("tick") {
                    continue;
                }
                let mut want: std::vec::Vec<Hung> = std::vec::Vec::new();
                for (i, &(at, kind, claim, publish)) in plan.iter().enumerate() {
                    let live = claim <= ms && publish.is_none_or(|p| p > ms);
                    if live && !named[i] && (ms - claim) * MS >= deadline(kind).0 {
                        named[i] = true;
                        want.push(Hung { at, by: TaskId(i as u32), kind, held: Nanos((ms - claim) * MS) });
                    }
                }
                want.sort_by_key(|h| (h.kind, h.at.0));
                let mut out: Vec<Hung, Global> = Vec::new_in(Global);
                watch.tick(Nanos(ms * MS), deadline, &mut out).map_err(|e| Broke::Oracle(format!("tick: {e}")))?;
                if !want.is_empty() {
                    tape.reached("a claim was named hung");
                }
                if out.as_slice() != want.as_slice() {
                    return Err(Broke::Oracle(format!("at {ms} ms the watch named {:?}, the model {want:?}", out.as_slice())));
                }
            }
            // The bounds are typed refusals.
            let mut full = ClaimWatch::new_in(cap, Global).map_err(|e| Broke::Harness(format!("{e}")))?;
            for i in 0..cap {
                full.claimed(At(i), TaskId(i), 0, Nanos(0)).map_err(|e| Broke::Oracle(format!("under cap: {e}")))?;
            }
            if full.claimed(At(cap), TaskId(cap), 0, Nanos(0)) != Err(WatchError::Full { cap, at: At(cap) }) {
                return Err(Broke::Oracle(format!("claim {} past cap {cap} was not Full", cap + 1)));
            }
            if full.published(At(cap + 1)) != Err(WatchError::NotWatched { at: At(cap + 1) }) {
                return Err(Broke::Oracle("a publish of an unknown slot was not NotWatched".into()));
            }
            Ok(())
        });
    }
}
