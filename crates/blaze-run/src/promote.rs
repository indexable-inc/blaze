//! [`Promote`]: from sampled heat to optimized code in the hot region (docs/v2/plan/design/
//! backend-next.md, D97 point 3 as decided by D100 points 4 and 13).
//!
//! **Heat by samples.** The launcher's sampler and its attribution (blaze-launch-rt
//! sample/attribute.rs) give, per sampling pass, piece 0's profile: samples by function and
//! sampled call edges, by symbol. The parent maps each symbol to the slot it declared for it and
//! hands the rows here by slot ([`SlotSamples`], [`SlotEdge`]): this crate is below blaze-tier and
//! does not see its types. A slot's samples add up over the run; the first pass at which they
//! reach [`heat::Config::bound`] hands the slot to [`Heat::reached`], which queues it once for the
//! run however often it is reached again: the same once-only the count stubs had, with nothing
//! counted in the function's code.
//!
//! **The hot region.** The optimizing tier ([`crate::tier::Tier`]) makes each hot slot's record;
//! each record made is [`Promote::ready`]. [`Promote::place`] then lays the ready batch out in the
//! hot region, a range of the code area of its own, in the order the sampled call edges give
//! (code-layout.md's C3, as blaze-link's layout.rs does it for a whole link: densest first, a
//! function joins its heaviest caller's cluster unless that call is no more than a tenth of its own
//! weight, the cluster would pass the maximum, or the merge would leave the cluster under an eighth
//! of its caller cluster's density; clusters densest first; every tie by slot). Each
//! [`Placement`] is then published by [`crate::slots::Slots::publish`] in the order given: one
//! store of the slot word names the new body (D100 point 13: no code is patched). A record laid
//! out in an earlier batch keeps its place: an edge to it orders nothing in a later batch.
//!
//! **Not shared.** A record optimized for a slot found hot by this run's samples was offered its
//! callees by an observed profile: its key holds the offer set's digest and it stays in the
//! private layer (D100 point 8; blaze-tier key.rs `OptimizedKeyIn::observed`), which is the
//! optimizer's to key, not this machine's.

#![blaze::sans_io]

use core::fmt;

use blaze_own::{ASortedMap, AVec, Alloc, SortedMap, Vec};

use crate::heat::{self, Heat};
use crate::{CodeRef, Plant, SlotId};

/// One slot's samples in one pass.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SlotSamples {
    pub slot: SlotId,
    pub samples: u32,
}

/// One sampled call edge in one pass.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SlotEdge {
    pub caller: SlotId,
    pub callee: SlotId,
    pub samples: u32,
}

/// The hot region's parameters: the caller's, never a constant inside.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    /// The hot region's length in bytes.
    pub hot_bytes: u64,
    /// Each record's alignment in it: a power of two.
    pub align: u64,
    /// The most bytes one cluster of callers and callees may hold (code-layout.md: 1 MiB).
    pub max_cluster: u64,
}

/// One optimized record laid out: publish it at `at` (bytes from the hot region's start), in the
/// order given.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Placement {
    pub slot: SlotId,
    pub code: CodeRef,
    pub at: u64,
    pub len: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    Config { what: &'static str, value: u64 },
    NoSlot { slot: SlotId, slots: u32 },
    /// A record made ready for a slot already laid out or already ready: the tier publishes once.
    Twice { slot: SlotId },
    /// A record of no bytes.
    Empty { slot: SlotId },
    /// The batch does not fit what is left of the hot region.
    HotFull { want: u64, left: u64 },
    Heat(heat::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Error::Config { what, value } => write!(f, "promotion's config: {what} ({value})"),
            Error::NoSlot { slot, slots } => {
                write!(f, "promotion was told of slot {}, past the table's {slots}", slot.0)
            }
            Error::Twice { slot } => write!(f, "slot {}'s optimized record was made ready twice: promoted twice", slot.0),
            Error::Empty { slot } => write!(f, "slot {}'s optimized record holds no code", slot.0),
            Error::HotFull { want, left } => {
                write!(f, "the hot region has {left} bytes left and the batch wants {want} (D100 point 13)")
            }
            Error::Heat(error) => write!(f, "promotion's heat: {error}"),
        }
    }
}

impl core::error::Error for Error {}

impl From<heat::Error> for Error {
    fn from(error: heat::Error) -> Error {
        Error::Heat(error)
    }
}

/// A record made, waiting for the next [`Promote::place`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Ready {
    slot: SlotId,
    code: CodeRef,
    len: u64,
}

/// One program's promotion. See the module's documentation.
pub struct Promote<'a> {
    alloc: Alloc<'a>,
    config: Config,
    heat: Heat<'a>,
    /// Samples a slot, summed over the run.
    samples: AVec<'a, u64>,
    /// Sampled edges summed over the run, keyed (callee, caller): a callee's callers are one range.
    edges: ASortedMap<'a, (SlotId, SlotId), u64>,
    ready: AVec<'a, Ready>,
    /// A flag a slot: laid out in the hot region (once, for the run).
    placed: AVec<'a, bool>,
    /// The hot region's next free byte.
    next: u64,
    pub(crate) plant: Plant,
}

impl<'a> Promote<'a> {
    pub fn new(slots: u32, heat: heat::Config, config: Config, alloc: Alloc<'a>) -> Result<Promote<'a>, Error> {
        if config.align == 0 || !config.align.is_power_of_two() {
            return Err(Error::Config { what: "the alignment is not a power of two", value: config.align });
        }
        if config.hot_bytes == 0 {
            return Err(Error::Config { what: "a hot region of no bytes", value: 0 });
        }
        if config.max_cluster == 0 {
            return Err(Error::Config { what: "a cluster maximum of no bytes", value: 0 });
        }
        Ok(Promote {
            alloc,
            config,
            heat: Heat::new(slots, heat, alloc)?,
            samples: Vec::from_elem_in(0, slots as usize, alloc),
            edges: SortedMap::new_in(alloc),
            ready: Vec::new_in(alloc),
            placed: Vec::from_elem_in(false, slots as usize, alloc),
            next: 0,
            plant: Plant::None,
        })
    }

    /// The bytes of the hot region laid out so far.
    pub fn used(&self) -> u64 {
        self.next
    }

    /// A slot's samples so far.
    pub fn samples(&self, slot: SlotId) -> Option<u64> {
        self.samples.get(slot.index()).copied()
    }

    fn check(&self, slot: SlotId) -> Result<(), Error> {
        let slots = self.samples.len() as u32;
        if slot.index() >= self.samples.len() {
            return Err(Error::NoSlot { slot, slots });
        }
        Ok(())
    }

    /// One sampling pass's rows and edges. Pushes to `hot` each slot that becomes hot, once for the
    /// run; give each to the optimizing tier (`tier::Tier::hot`). Every slot is checked before
    /// anything is counted, so a refused pass changes nothing.
    pub fn sampled(&mut self, rows: &[SlotSamples], edges: &[SlotEdge], hot: &mut AVec<'_, SlotId>) -> Result<(), Error> {
        for row in rows {
            self.check(row.slot)?;
        }
        for edge in edges {
            self.check(edge.caller)?;
            self.check(edge.callee)?;
        }
        let bound = u64::from(self.heat.bound());
        for row in rows {
            let count = &mut self.samples[row.slot.index()];
            *count = count.saturating_add(u64::from(row.samples));
            if *count >= bound {
                // Heat queues the slot the first time only.
                self.heat.reached(row.slot, hot)?;
            }
        }
        for edge in edges {
            let sum = self.edges.entry((edge.callee, edge.caller)).or_insert(0);
            *sum = sum.saturating_add(u64::from(edge.samples));
        }
        Ok(())
    }

    /// The optimizing tier made `code`, `len` bytes, for `slot` (its `Effect::Publish`): laid out
    /// at the next [`Promote::place`].
    pub fn ready(&mut self, slot: SlotId, code: CodeRef, len: u64) -> Result<(), Error> {
        self.check(slot)?;
        if len == 0 {
            return Err(Error::Empty { slot });
        }
        if self.placed[slot.index()] || self.ready.iter().any(|it| it.slot == slot) {
            return Err(Error::Twice { slot });
        }
        self.ready.push(Ready { slot, code, len });
        Ok(())
    }

    /// Lays the ready batch out in the hot region in the sampled edges' order and pushes each
    /// [`Placement`] to `out`, in the order to publish them. All or nothing: a batch that does not
    /// fit is refused whole and stays ready.
    pub fn place(&mut self, out: &mut AVec<'_, Placement>) -> Result<(), Error> {
        let order = self.order();
        // Offsets first, committed only once the whole batch fits.
        let mut next = self.next;
        let mut laid: AVec<'a, (usize, u64)> = Vec::with_capacity_in(order.len(), self.alloc);
        for &at in order.iter() {
            let len = self.ready[at].len;
            let start = next.next_multiple_of(self.config.align);
            let end = start.checked_add(len).filter(|&end| end <= self.config.hot_bytes);
            let Some(end) = end else {
                let want: u64 = order.iter().map(|&it| self.ready[it].len).sum();
                return Err(Error::HotFull { want, left: self.config.hot_bytes.saturating_sub(self.next) });
            };
            laid.push((at, start));
            next = end;
        }
        for &(at, start) in laid.iter() {
            let Ready { slot, code, len } = self.ready[at];
            self.placed[slot.index()] = true;
            out.push(Placement { slot, code, at: start, len });
        }
        self.next = next;
        if self.plant != Plant::PromoteTwice {
            self.ready.clear();
        }
        Ok(())
    }

    /// The ready batch's order, as indices into `ready` (see the module's documentation).
    fn order(&self) -> AVec<'a, usize> {
        let count = self.ready.len();
        let weight = |at: usize| self.samples[self.ready[at].slot.index()];
        // The batch's slots, sorted: an edge's caller is found in the batch by a search.
        let mut by_slot: AVec<'a, (SlotId, usize)> = Vec::with_capacity_in(count, self.alloc);
        by_slot.extend(self.ready.iter().enumerate().map(|(at, it)| (it.slot, at)));
        by_slot.sort_unstable();
        let mut nodes: AVec<'a, usize> = Vec::with_capacity_in(count, self.alloc);
        nodes.extend(0..count);
        nodes.sort_unstable_by(|&a, &b| {
            denser((weight(b), self.ready[b].len), (weight(a), self.ready[a].len))
                .then(self.ready[a].slot.cmp(&self.ready[b].slot))
        });
        // Each node's cluster: its leader, and per leader the members as a list (head, tail, next)
        // with the cluster's bytes and weight.
        let mut leader: AVec<'a, usize> = Vec::with_capacity_in(count, self.alloc);
        leader.extend(0..count);
        let mut head: AVec<'a, usize> = Vec::with_capacity_in(count, self.alloc);
        head.extend(0..count);
        let mut tail: AVec<'a, usize> = Vec::with_capacity_in(count, self.alloc);
        tail.extend(0..count);
        let mut next: AVec<'a, Option<usize>> = Vec::from_elem_in(None, count, self.alloc);
        let mut bytes: AVec<'a, u64> = Vec::with_capacity_in(count, self.alloc);
        bytes.extend(self.ready.iter().map(|it| it.len));
        let mut weights: AVec<'a, u64> = Vec::with_capacity_in(count, self.alloc);
        weights.extend((0..count).map(weight));
        for &node in nodes.iter() {
            let callee = self.ready[node].slot;
            // The heaviest caller in the batch; ties to the lower slot (the range is by caller).
            let mut best: Option<(u64, usize)> = None;
            for (&(_, caller), &samples) in self.edges.range((callee, SlotId(0))..=(callee, SlotId(u32::MAX))) {
                if caller == callee {
                    continue;
                }
                let Ok(found) = by_slot.binary_search_by_key(&caller, |&(slot, _)| slot) else { continue };
                let at = by_slot[found].1;
                if best.is_none_or(|(most, _)| samples > most) {
                    best = Some((samples, at));
                }
            }
            let Some((call, caller)) = best else { continue };
            // A call no more than a tenth of the callee's own weight does not pull it.
            if u128::from(call) * 10 <= u128::from(weight(node)) {
                continue;
            }
            let into = leader_of(&leader, caller);
            let from = leader_of(&leader, node);
            if into == from {
                continue;
            }
            let merged = bytes[into].saturating_add(bytes[from]);
            if merged > self.config.max_cluster {
                continue;
            }
            // The merged density under an eighth of the caller cluster's: (Wc + Wm) / (Sc + Sm) <
            // Wc / (8 Sc), compared as exact products.
            let merged_weight = u128::from(weights[into]) + u128::from(weights[from]);
            if merged_weight * u128::from(bytes[into]) * 8 < u128::from(weights[into]) * u128::from(merged) {
                continue;
            }
            next[tail[into]] = Some(head[from]);
            tail[into] = tail[from];
            bytes[into] = merged;
            weights[into] = weights[into].saturating_add(weights[from]);
            leader[from] = into;
        }
        let mut clusters: AVec<'a, usize> = Vec::with_capacity_in(count, self.alloc);
        clusters.extend((0..count).filter(|&at| leader[at] == at));
        clusters.sort_unstable_by(|&a, &b| {
            denser((weights[b], bytes[b]), (weights[a], bytes[a]))
                .then(self.ready[head[a]].slot.cmp(&self.ready[head[b]].slot))
        });
        let mut order: AVec<'a, usize> = Vec::with_capacity_in(count, self.alloc);
        for &cluster in clusters.iter() {
            let mut at = Some(head[cluster]);
            while let Some(member) = at {
                order.push(member);
                at = next[member];
            }
        }
        order
    }
}

/// `a`'s density (weight over bytes) against `b`'s, as exact products.
fn denser(a: (u64, u64), b: (u64, u64)) -> core::cmp::Ordering {
    (u128::from(a.0) * u128::from(b.1)).cmp(&(u128::from(b.0) * u128::from(a.1)))
}

/// The leader of `at`'s cluster: a merged leader points at the cluster it joined.
fn leader_of(leader: &[usize], mut at: usize) -> usize {
    while leader[at] != at {
        at = leader[at];
    }
    at
}
