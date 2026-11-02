//! Vector clocks, ported from Miri's `src/tools/miri/src/concurrency/vector_clock.rs`.
//!
//! A timestamp carries the [`SiteId`] of the access that last set it instead of Miri's `Span`,
//! so a timestamp is 8 bytes and a clock of up to four threads lives inline.

use std::{
    cmp::Ordering,
    ops::{Index, Shr},
};

use smallvec::SmallVec;

use crate::ids::SiteId;

/// A vector clock index: a thread's slot in every clock. One index may be reused by a later
/// thread once nothing can race with the thread that held it (Miri: `data_race.rs`,
/// `find_vector_index_reuse_candidate`).
#[derive(Clone, Copy, Debug, PartialOrd, Ord, PartialEq, Eq, Hash)]
pub(super) struct VectorIdx(pub(super) u32);

impl VectorIdx {
    #[inline(always)]
    pub(super) fn index(self) -> usize {
        self.0 as usize
    }
}

/// The kind of a non-atomic read (Miri: `data_race.rs`, `NaReadType`).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(super) enum NaReadType {
    Read,
    /// An implicit read of a retag.
    Retag,
}

/// The inline size of a clock; larger ones spill to the heap (Miri: `SMALL_VECTOR`).
const SMALL_VECTOR: usize = 4;

/// A thread's time and the site that set it (Miri: `VTimestamp`). Compared by time alone.
#[derive(Clone, Copy)]
pub(super) struct VTimestamp {
    /// The lowest bit is the read type (`1`: a retag read), the rest the time.
    time_and_read_type: u32,
    pub(super) site: SiteId,
}

const _: () = assert!(size_of::<VTimestamp>() == 8);

impl std::fmt::Debug for VTimestamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VTimestamp")
            .field("time", &self.time())
            .field("read_type", &self.read_type())
            .field("site", &self.site)
            .finish()
    }
}

/// Miri's `Span::is_dummy`: the site that says nothing.
#[inline(always)]
pub(super) fn is_dummy(site: SiteId) -> bool {
    site == SiteId::START
}

/// Miri's `Span::substitute_dummy`.
#[inline(always)]
fn substitute_dummy(site: SiteId, other: SiteId) -> SiteId {
    if is_dummy(site) { other } else { site }
}

impl VTimestamp {
    pub(super) const ZERO: VTimestamp = VTimestamp::new(0, NaReadType::Read, SiteId::START);

    #[inline]
    const fn encode_time_and_read_type(time: u32, read_type: NaReadType) -> u32 {
        let read_type_bit = match read_type {
            NaReadType::Read => 0,
            NaReadType::Retag => 1,
        };
        read_type_bit | time.checked_mul(2).expect("Vector clock overflow")
    }

    #[inline]
    pub(super) const fn new(time: u32, read_type: NaReadType, site: SiteId) -> Self {
        Self { time_and_read_type: Self::encode_time_and_read_type(time, read_type), site }
    }

    #[inline]
    pub(super) fn time(&self) -> u32 {
        self.time_and_read_type.shr(1)
    }

    #[inline]
    fn set_time(&mut self, time: u32) {
        self.time_and_read_type = Self::encode_time_and_read_type(time, self.read_type());
    }

    #[inline]
    pub(super) fn read_type(&self) -> NaReadType {
        if self.time_and_read_type & 1 == 0 { NaReadType::Read } else { NaReadType::Retag }
    }

    #[inline]
    pub(super) fn set_read_type(&mut self, read_type: NaReadType) {
        self.time_and_read_type = Self::encode_time_and_read_type(self.time(), read_type);
    }
}

impl PartialEq for VTimestamp {
    fn eq(&self, other: &Self) -> bool {
        self.time() == other.time()
    }
}

impl Eq for VTimestamp {}

impl PartialOrd for VTimestamp {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for VTimestamp {
    fn cmp(&self, other: &Self) -> Ordering {
        self.time().cmp(&other.time())
    }
}

/// A vector clock: a timestamp per vector index (Miri: `VClock`). The last element is never
/// zero, so the all-zero clock is the empty one and each clock has one representation, which
/// makes the derived equality right and lets comparisons stop at the shorter length.
#[derive(PartialEq, Eq, Default, Debug)]
pub(super) struct VClock(SmallVec<[VTimestamp; SMALL_VECTOR]>);

impl VClock {
    /// All zeros but `timestamp` at `index`.
    pub(super) fn new_with_index(index: VectorIdx, timestamp: VTimestamp) -> VClock {
        if timestamp.time() == 0 {
            return VClock::default();
        }
        let len = index.index() + 1;
        let mut vec = smallvec::smallvec![VTimestamp::ZERO; len];
        vec[index.index()] = timestamp;
        VClock(vec)
    }

    #[inline]
    pub(super) fn as_slice(&self) -> &[VTimestamp] {
        debug_assert!(self.0.last().is_none_or(|t| t.time() != 0));
        self.0.as_slice()
    }

    /// The timestamp at `index`, which must exist.
    #[inline]
    pub(super) fn index_mut(&mut self, index: VectorIdx) -> &mut VTimestamp {
        self.0.as_mut_slice().get_mut(index.index()).unwrap()
    }

    /// The timestamps, at least `min_len` of them; the caller makes element `min_len - 1`
    /// non-zero to keep the invariant.
    #[inline]
    fn get_mut_with_min_len(&mut self, min_len: usize) -> &mut [VTimestamp] {
        if self.0.len() < min_len {
            self.0.resize(min_len, VTimestamp::ZERO);
        }
        self.0.as_mut_slice()
    }

    /// One more at `idx`; a non-dummy `site` becomes the timestamp's site.
    #[inline]
    pub(super) fn increment_index(&mut self, idx: VectorIdx, site: SiteId) {
        let idx = idx.index();
        let mut_slice = self.get_mut_with_min_len(idx + 1);
        let idx_ref = &mut mut_slice[idx];
        idx_ref.set_time(idx_ref.time().checked_add(1).expect("Vector clock overflow"));
        if !is_dummy(site) {
            idx_ref.site = site;
        }
    }

    /// The elementwise maximum of the two.
    pub(super) fn join(&mut self, other: &Self) {
        let rhs_slice = other.as_slice();
        let lhs_slice = self.get_mut_with_min_len(rhs_slice.len());
        for (l, &r) in lhs_slice.iter_mut().zip(rhs_slice.iter()) {
            let l_site = l.site;
            let r_site = r.site;
            *l = r.max(*l);
            l.site = substitute_dummy(substitute_dummy(l.site, r_site), l_site);
        }
    }

    /// The element at `idx` set to `other`'s. May only increase it.
    pub(super) fn set_at_index(&mut self, other: &Self, idx: VectorIdx) {
        let new_timestamp = other[idx];
        // Setting to 0 is different, since the last element cannot be 0: past the end there is
        // nothing to do, and inside it only increases, so the last element stays non-zero.
        if new_timestamp.time() == 0 && idx.index() >= self.0.len() {
            return;
        }

        let mut_slice = self.get_mut_with_min_len(idx.index() + 1);
        let mut_timestamp = &mut mut_slice[idx.index()];

        let prev_site = mut_timestamp.site;

        assert!(*mut_timestamp <= new_timestamp, "set_at_index: may only increase the timestamp");
        *mut_timestamp = new_timestamp;

        mut_timestamp.site = substitute_dummy(mut_timestamp.site, prev_site);
    }

    #[inline]
    pub(super) fn set_zero_vector(&mut self) {
        self.0.clear();
    }

    /// The first index where `self` is greater than `other`, if any (Miri: `data_race.rs`,
    /// `VClockAlloc::find_gt_index`).
    pub(super) fn find_gt_index(&self, other: &VClock) -> Option<VectorIdx> {
        let l_slice = self.as_slice();
        let r_slice = other.as_slice();
        l_slice
            .iter()
            .zip(r_slice.iter())
            .position(|(&l, &r)| l > r)
            .or_else(|| {
                if l_slice.len() > r_slice.len() {
                    // By the invariant, one element of the longer tail is non-zero.
                    let idx = l_slice[r_slice.len()..]
                        .iter()
                        .position(|&t| t != VTimestamp::ZERO)
                        .expect("Invalid VClock Invariant");
                    Some(idx + r_slice.len())
                } else {
                    None
                }
            })
            .map(|idx| VectorIdx(idx as u32))
    }
}

impl Clone for VClock {
    fn clone(&self) -> Self {
        VClock(self.0.clone())
    }

    fn clone_from(&mut self, source: &Self) {
        self.0.clear();
        self.0.extend_from_slice(source.as_slice());
    }
}

impl PartialOrd for VClock {
    fn partial_cmp(&self, other: &VClock) -> Option<Ordering> {
        let lhs_slice = self.as_slice();
        let rhs_slice = other.as_slice();

        // `order` is the comparison of the prefixes seen so far: Equal turns into Less or
        // Greater at the first difference, and a difference the other way means no order.
        let mut iter = lhs_slice.iter().zip(rhs_slice.iter());
        let mut order = match iter.next() {
            Some((lhs, rhs)) => lhs.cmp(rhs),
            None => Ordering::Equal,
        };
        for (l, r) in iter {
            match order {
                Ordering::Equal => order = l.cmp(r),
                Ordering::Less => {
                    if l > r {
                        return None;
                    }
                }
                Ordering::Greater => {
                    if l < r {
                        return None;
                    }
                }
            }
        }

        // A longer tail has a non-zero element by the invariant.
        let l_len = lhs_slice.len();
        let r_len = rhs_slice.len();
        match l_len.cmp(&r_len) {
            Ordering::Equal => Some(order),
            Ordering::Less => match order {
                Ordering::Less | Ordering::Equal => Some(Ordering::Less),
                Ordering::Greater => None,
            },
            Ordering::Greater => match order {
                Ordering::Greater | Ordering::Equal => Some(Ordering::Greater),
                Ordering::Less => None,
            },
        }
    }

    fn lt(&self, other: &VClock) -> bool {
        let lhs_slice = self.as_slice();
        let rhs_slice = other.as_slice();
        let l_len = lhs_slice.len();
        let r_len = rhs_slice.len();
        if l_len <= r_len {
            let mut equal = l_len == r_len;
            for (&l, &r) in lhs_slice.iter().zip(rhs_slice.iter()) {
                if l > r {
                    return false;
                } else if l < r {
                    equal = false;
                }
            }
            !equal
        } else {
            false
        }
    }

    fn le(&self, other: &VClock) -> bool {
        let lhs_slice = self.as_slice();
        let rhs_slice = other.as_slice();
        let l_len = lhs_slice.len();
        let r_len = rhs_slice.len();
        if l_len <= r_len {
            !lhs_slice.iter().zip(rhs_slice.iter()).any(|(&l, &r)| l > r)
        } else {
            false
        }
    }

    fn gt(&self, other: &VClock) -> bool {
        let lhs_slice = self.as_slice();
        let rhs_slice = other.as_slice();
        let l_len = lhs_slice.len();
        let r_len = rhs_slice.len();
        if l_len >= r_len {
            let mut equal = l_len == r_len;
            for (&l, &r) in lhs_slice.iter().zip(rhs_slice.iter()) {
                if l < r {
                    return false;
                } else if l > r {
                    equal = false;
                }
            }
            !equal
        } else {
            false
        }
    }

    fn ge(&self, other: &VClock) -> bool {
        let lhs_slice = self.as_slice();
        let rhs_slice = other.as_slice();
        let l_len = lhs_slice.len();
        let r_len = rhs_slice.len();
        if l_len >= r_len {
            !lhs_slice.iter().zip(rhs_slice.iter()).any(|(&l, &r)| l < r)
        } else {
            false
        }
    }
}

impl Index<VectorIdx> for VClock {
    type Output = VTimestamp;

    #[inline]
    fn index(&self, index: VectorIdx) -> &VTimestamp {
        self.as_slice().get(index.index()).unwrap_or(&VTimestamp::ZERO)
    }
}

#[cfg(test)]
impl VClock {
    /// A clock of these times, trailing zeros dropped (Miri's tests: `from_slice`).
    pub(super) fn from_times(mut slice: &[u32]) -> VClock {
        while let Some(0) = slice.last() {
            slice = &slice[..slice.len() - 1]
        }
        VClock(
            slice
                .iter()
                .map(|&time| VTimestamp::new(time, NaReadType::Read, SiteId::START))
                .collect(),
        )
    }

    pub(super) fn last_time(&self) -> Option<u32> {
        self.0.last().map(VTimestamp::time)
    }
}
