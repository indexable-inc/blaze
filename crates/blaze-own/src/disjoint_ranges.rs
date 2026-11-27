//! [`DisjointRanges`]: a zero-allocation borrowed proof wrapper over `&'a [(u32, u32)]` holding
//! sorted, strictly disjoint inclusive intervals `[start, end]`.
//!
//! Enforces the sorted-disjoint invariant once at construction (`try_from_sorted_disjoint`) so
//! repeated overlap queries (`overlaps`) run in O(1) bounding-box reject + O(|small| log |large|)
//! asymmetric binary search / linear two-pointer merge without re-verifying or scanning linearly
//! on every query (Rule 73, Rule 74).

use core::slice;

/// A borrowed slice of sorted, strictly disjoint inclusive `[start, end]` intervals over `u32`.
///
/// # Invariants
///
/// - Every entry `(start, end)` satisfies `start <= end`.
/// - Every adjacent pair `(s0, e0), (s1, e1)` satisfies `e0 < s1` (strictly ascending and disjoint).
///
/// Zero allocation, `#![no_std]`, zero `unsafe`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DisjointRanges<'a>(&'a [(u32, u32)]);

impl<'a> DisjointRanges<'a> {
    /// An empty range set.
    pub const EMPTY: Self = Self(&[]);

    /// Returns an empty [`DisjointRanges`].
    #[inline]
    pub const fn empty() -> Self {
        Self(&[])
    }

    /// Verifies in O(N) that every interval has `start <= end` and every consecutive pair is
    /// strictly ascending and disjoint (`prev.1 < next.0`), returning the proof wrapper or `None`.
    #[blaze::cost(O(n), space = O(1))]
    #[inline]
    pub fn try_from_sorted_disjoint(slice: &'a [(u32, u32)]) -> Option<Self> {
        let mut prev_end: Option<u32> = None;
        for &(start, end) in slice {
            if start > end {
                return None;
            }
            if let Some(pe) = prev_end
                && start <= pe
            {
                return None;
            }
            prev_end = Some(end);
        }
        Some(Self(slice))
    }

    /// Returns the number of disjoint intervals.
    #[inline]
    pub const fn len(self) -> usize {
        self.0.len()
    }

    /// Returns `true` if there are no intervals.
    #[inline]
    pub const fn is_empty(self) -> bool {
        self.0.is_empty()
    }

    /// Returns the underlying slice of sorted disjoint `[start, end]` intervals.
    #[inline]
    pub const fn as_slice(self) -> &'a [(u32, u32)] {
        self.0
    }

    /// Returns the first interval, if non-empty.
    #[inline]
    pub fn first(self) -> Option<(u32, u32)> {
        self.0.first().copied()
    }

    /// Returns the last interval, if non-empty.
    #[inline]
    pub fn last(self) -> Option<(u32, u32)> {
        self.0.last().copied()
    }

    /// Returns an iterator over the sorted disjoint intervals.
    #[inline]
    pub fn iter(self) -> slice::Iter<'a, (u32, u32)> {
        self.0.iter()
    }

    /// Returns `true` if `point` falls within any interval in O(log N).
    #[blaze::cost(O(log n), space = O(1))]
    #[inline]
    pub fn contains_point(self, point: u32) -> bool {
        let i = self.0.partition_point(|&(_, end)| end < point);
        self.0.get(i).is_some_and(|&(start, _)| start <= point)
    }

    /// Returns `true` if any interval in `self` shares at least one point with any interval in
    /// `other`.
    ///
    /// Uses O(1) bounding-box rejection, O(|small| log |large|) asymmetric binary search when
    /// lengths differ sharply, and a linear two-pointer merge when lengths are comparable.
    #[blaze::cost(O(n), space = O(1))]
    #[inline]
    pub fn overlaps(self, other: Self) -> bool {
        let a = self.0;
        let b = other.0;
        let (Some(&(a0_s, a0_e)), Some(&(b0_s, b0_e))) = (a.first(), b.first()) else {
            return false;
        };
        let a_last_e = a[a.len() - 1].1;
        let b_last_e = b[b.len() - 1].1;
        if a_last_e < b0_s || b_last_e < a0_s {
            return false;
        }
        if a.len() == 1 {
            let j = b.partition_point(|&(_, b_e)| b_e < a0_s);
            return b.get(j).is_some_and(|&(b_s, _)| b_s <= a0_e);
        }
        if b.len() == 1 {
            let i = a.partition_point(|&(_, a_e)| a_e < b0_s);
            return a.get(i).is_some_and(|&(a_s, _)| a_s <= b0_e);
        }
        if b.len() > a.len().saturating_mul(4) {
            let mut j = 0usize;
            for &(a_s, a_e) in a {
                j += b[j..].partition_point(|&(_, b_e)| b_e < a_s);
                let Some(&(b_s, _)) = b.get(j) else {
                    return false;
                };
                if b_s <= a_e {
                    return true;
                }
            }
            return false;
        }
        if a.len() > b.len().saturating_mul(4) {
            let mut i = 0usize;
            for &(b_s, b_e) in b {
                i += a[i..].partition_point(|&(_, a_e)| a_e < b_s);
                let Some(&(a_s, _)) = a.get(i) else {
                    return false;
                };
                if a_s <= b_e {
                    return true;
                }
            }
            return false;
        }
        let mut i = 0usize;
        let mut j = 0usize;
        while let (Some(&(a_s, a_e)), Some(&(b_s, b_e))) = (a.get(i), b.get(j)) {
            if a_e < b_s {
                i += 1;
            } else if b_e < a_s {
                j += 1;
            } else {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::DisjointRanges;

    fn naive_overlaps(a: &[(u32, u32)], b: &[(u32, u32)]) -> bool {
        a.iter().any(|&(a_s, a_e)| b.iter().any(|&(b_s, b_e)| a_s <= b_e && b_s <= a_e))
    }

    #[test]
    fn try_from_sorted_disjoint_validates_invariants() {
        assert_eq!(DisjointRanges::try_from_sorted_disjoint(&[]), Some(DisjointRanges::EMPTY));
        assert!(DisjointRanges::try_from_sorted_disjoint(&[(0, 0)]).is_some());
        assert!(DisjointRanges::try_from_sorted_disjoint(&[(0, 3), (4, 7), (10, u32::MAX)]).is_some());

        // Inverted interval (`start > end`).
        assert_eq!(DisjointRanges::try_from_sorted_disjoint(&[(5, 4)]), None);
        assert_eq!(DisjointRanges::try_from_sorted_disjoint(&[(0, 2), (5, 4)]), None);

        // Touching / overlapping endpoints (`e0 >= s1`).
        assert_eq!(DisjointRanges::try_from_sorted_disjoint(&[(0, 3), (3, 5)]), None);
        assert_eq!(DisjointRanges::try_from_sorted_disjoint(&[(0, 5), (2, 7)]), None);
        assert_eq!(DisjointRanges::try_from_sorted_disjoint(&[(0, u32::MAX), (u32::MAX, u32::MAX)]), None);

        // Unsorted intervals.
        assert_eq!(DisjointRanges::try_from_sorted_disjoint(&[(10, 12), (1, 2)]), None);
    }

    #[test]
    fn contains_point_and_accessors() {
        let raw = [(2u32, 4u32), (5, 5), (10, 15)];
        let r = DisjointRanges::try_from_sorted_disjoint(&raw).expect("valid disjoint ranges");
        assert_eq!(r.len(), 3);
        assert!(!r.is_empty());
        assert_eq!(r.first(), Some((2, 4)));
        assert_eq!(r.last(), Some((10, 15)));
        assert!(!r.contains_point(1));
        assert!(r.contains_point(2));
        assert!(r.contains_point(4));
        assert!(r.contains_point(5));
        assert!(!r.contains_point(6));
        assert!(r.contains_point(10));
        assert!(r.contains_point(15));
        assert!(!r.contains_point(16));
    }

    #[test]
    fn overlaps_matches_quadratic_oracle_across_all_shapes() {
        let long_raw: std::vec::Vec<(u32, u32)> = (0..100).map(|i| (i * 10, i * 10 + 3)).collect();
        let long = DisjointRanges::try_from_sorted_disjoint(&long_raw).expect("long is valid");

        let adjacent_raw: [(u32, u32); 4] = [(0, 3), (4, 7), (8, 11), (12, 15)];
        let adjacent = DisjointRanges::try_from_sorted_disjoint(&adjacent_raw).expect("adjacent is valid");

        let max_edge_raw: [(u32, u32); 2] = [(0, 0), (u32::MAX - 2, u32::MAX)];
        let max_edge = DisjointRanges::try_from_sorted_disjoint(&max_edge_raw).expect("max edge is valid");

        let probes: [&[(u32, u32)]; 12] = [
            &[],
            &[(4, 9)],
            &[(990, 990)],
            &[(994, 1050)],
            &[(4, 9), (14, 19), (994, 999)],
            &[(4, 9), (503, 505), (994, 999)],
            &[(0, 0), (10, 10)],
            &[(3, 10)],
            &[(5, 8), (15, 18), (25, 28), (35, 38), (45, 48), (55, 58)],
            &[(0, 3), (4, 7)],
            &[(u32::MAX - 3, u32::MAX - 3)],
            &[(u32::MAX, u32::MAX)],
        ];

        for p_raw in probes {
            let p = DisjointRanges::try_from_sorted_disjoint(p_raw).expect("probe is valid");
            assert_eq!(p.overlaps(long), naive_overlaps(p_raw, &long_raw), "p vs long: {p_raw:?}");
            assert_eq!(long.overlaps(p), naive_overlaps(&long_raw, p_raw), "long vs p: {p_raw:?}");
            assert_eq!(p.overlaps(adjacent), naive_overlaps(p_raw, &adjacent_raw), "p vs adj: {p_raw:?}");
            assert_eq!(adjacent.overlaps(p), naive_overlaps(&adjacent_raw, p_raw), "adj vs p: {p_raw:?}");
            assert_eq!(p.overlaps(max_edge), naive_overlaps(p_raw, &max_edge_raw), "p vs max: {p_raw:?}");
            assert_eq!(max_edge.overlaps(p), naive_overlaps(&max_edge_raw, p_raw), "max vs p: {p_raw:?}");
            for q_raw in probes {
                let q = DisjointRanges::try_from_sorted_disjoint(q_raw).expect("q is valid");
                assert_eq!(p.overlaps(q), naive_overlaps(p_raw, q_raw), "p vs q: {p_raw:?} vs {q_raw:?}");
            }
        }
    }
}
