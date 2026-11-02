//! A set of points as sorted ranges, as rustc's `IntervalSet`: what a region holds is a few
//! runs of consecutive points (a local is live from where it is assigned to its last use), so
//! a region costs its runs, not the body's size.

use crate::mem::{FVec, Frame};

/// Sorted, disjoint, non-adjacent inclusive ranges of points.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct IntervalSet<'f> {
    ranges: FVec<'f, (u32, u32)>,
}

fn pack((start, end): (u32, u32)) -> u64 {
    (u64::from(start) << 32) | u64::from(end)
}

fn unpack(it: u64) -> (u32, u32) {
    ((it >> 32) as u32, it as u32)
}

impl<'f> IntervalSet<'f> {
    pub(super) fn new(frame: &'f Frame) -> Self {
        IntervalSet { ranges: Vec::new_in(frame) }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// The index of the first range that ends at or after `point`.
    fn first_ending_at_or_after(&self, point: u32) -> usize {
        self.ranges.partition_point(|&(_, end)| end < point)
    }

    pub(super) fn contains(&self, point: usize) -> bool {
        let point = point as u32;
        let at = self.first_ending_at_or_after(point);
        self.ranges.get(at).is_some_and(|&(start, _)| start <= point)
    }

    pub(super) fn insert(&mut self, point: usize) -> bool {
        self.insert_range(point, point)
    }

    /// Adds the points `start..=end`, and says whether that added any.
    pub(super) fn insert_range(&mut self, start: usize, end: usize) -> bool {
        let (start, end) = (start as u32, end as u32);
        debug_assert!(start <= end);
        // Appending past the last range is the common case: liveness walks backwards per
        // block, but blocks are numbered and unioned in order.
        if let Some(last) = self.ranges.last_mut() {
            if start > last.1.saturating_add(1) {
                self.ranges.push((start, end));
                return true;
            }
            if start >= last.0 && end <= last.1 {
                return false;
            }
        } else {
            self.ranges.push((start, end));
            return true;
        }
        // The ranges that touch or are adjacent to `start..=end` merge with it.
        let first = self.ranges.partition_point(|&(_, e)| e.saturating_add(1) < start);
        let last = self.ranges.partition_point(|&(s, _)| s <= end.saturating_add(1));
        if first == last {
            self.ranges.insert(first, (start, end));
            return true;
        }
        let merged = (start.min(self.ranges[first].0), end.max(self.ranges[last - 1].1));
        if last - first == 1 && self.ranges[first] == merged {
            return false;
        }
        self.ranges.drain(first + 1..last);
        self.ranges[first] = merged;
        true
    }

    /// Adds the points of `other`, and says whether that added any.
    pub(super) fn union(&mut self, other: &IntervalSet<'_>) -> bool {
        if other.ranges.is_empty() {
            return false;
        }
        if self.ranges.is_empty() {
            self.ranges.extend_from_slice(&other.ranges);
            return true;
        }
        if other.ranges.len() <= 2 {
            let mut changed = false;
            for &(start, end) in &other.ranges {
                changed |= self.insert_range(start as usize, end as usize);
            }
            return changed;
        }
        // A merge of the two sorted lists.
        blaze_mem::scratch::with_u64s(|out| {
            out.reserve(self.ranges.len() + other.ranges.len());
            let (mut a, mut b) = (self.ranges.iter().peekable(), other.ranges.iter().peekable());
            loop {
                let next = match (a.peek(), b.peek()) {
                    (Some(&&x), Some(&&y)) => {
                        if x.0 <= y.0 {
                            a.next();
                            x
                        } else {
                            b.next();
                            y
                        }
                    }
                    (Some(&&x), None) => {
                        a.next();
                        x
                    }
                    (None, Some(&&y)) => {
                        b.next();
                        y
                    }
                    (None, None) => break,
                };
                match out.last_mut() {
                    Some(last) if next.0 <= unpack(*last).1.saturating_add(1) => {
                        let (start, end) = unpack(*last);
                        *last = pack((start, end.max(next.1)));
                    }
                    _ => out.push(pack(next)),
                }
            }
            let changed = out.len() != self.ranges.len()
                || out.iter().zip(&self.ranges).any(|(&it, &range)| unpack(it) != range);
            if changed {
                self.ranges.clear();
                self.ranges.extend(out.iter().map(|&it| unpack(it)));
            }
            changed
        })
    }

    /// Removes the points of `other`.
    pub(super) fn subtract(&mut self, other: &IntervalSet<'_>) {
        if self.ranges.is_empty() || other.ranges.is_empty() {
            return;
        }
        blaze_mem::scratch::with_u64s(|out| {
            let mut cut = other.ranges.iter().peekable();
            for &(mut start, end) in &self.ranges {
                let mut left = true;
                while let Some(&&(cs, ce)) = cut.peek() {
                    if ce < start {
                        cut.next();
                        continue;
                    }
                    if cs > end {
                        break;
                    }
                    if cs > start {
                        out.push(pack((start, cs - 1)));
                    }
                    if ce >= end {
                        left = false;
                        break;
                    }
                    start = ce + 1;
                    cut.next();
                }
                if left {
                    out.push(pack((start, end)));
                }
            }
            self.ranges.clear();
            self.ranges.extend(out.iter().map(|&it| unpack(it)));
        });
    }

    /// The first point of `start..=end` not in the set, if there is one.
    pub(super) fn first_absent_in(&self, start: usize, end: usize) -> Option<usize> {
        let (start, end) = (start as u32, end as u32);
        let at = self.first_ending_at_or_after(start);
        match self.ranges.get(at) {
            Some(&(s, e)) if s <= start => (e < end).then(|| e as usize + 1),
            _ => Some(start as usize),
        }
    }

    /// The runs of `start..=end` in the set, clipped to it.
    pub(super) fn runs_in(
        &self,
        start: usize,
        end: usize,
    ) -> impl Iterator<Item = (usize, usize)> + '_ {
        let (start, end) = (start as u32, end as u32);
        let at = self.first_ending_at_or_after(start);
        self.ranges[at..]
            .iter()
            .take_while(move |&&(s, _)| s <= end)
            .map(move |&(s, e)| (s.max(start) as usize, e.min(end) as usize))
    }

    /// Adds `start..=end`, and puts in `new` (emptied first) the runs of it that were not in
    /// the set before.
    pub(super) fn insert_new(
        &mut self,
        start: usize,
        end: usize,
        new: &mut FVec<'_, (usize, usize)>,
    ) {
        new.clear();
        let mut next = start;
        for (s, e) in self.runs_in(start, end) {
            if s > next {
                new.push((next, s - 1));
            }
            next = e + 1;
        }
        if next <= end {
            new.push((next, end));
        }
        if !new.is_empty() {
            self.insert_range(start, end);
        }
    }

    pub(super) fn clear(&mut self) {
        self.ranges.clear();
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.ranges.iter().flat_map(|&(start, end)| start as usize..=end as usize)
    }
}

#[cfg(test)]
mod tests {
    use blaze_mem::{Tag, frame};

    use super::IntervalSet;
    use crate::mem::Frame;

    fn set<'f>(frame: &'f Frame, points: &[usize]) -> IntervalSet<'f> {
        let mut out = IntervalSet::new(frame);
        for &it in points {
            out.insert(it);
        }
        out
    }

    #[test]
    fn insert_merges_and_keeps_order() {
        frame::task(Tag::Mir, |frame| {
            let s = set(frame, &[5, 1, 3, 2, 9, 4, 10]);
            assert_eq!(s.ranges.as_slice(), &[(1, 5), (9, 10)]);
            assert!(s.contains(3) && !s.contains(6) && s.contains(10) && !s.contains(0));
            let mut t = s.clone();
            assert!(!t.insert_range(2, 4));
            assert!(t.insert_range(6, 8));
            assert_eq!(t.ranges.as_slice(), &[(1, 10)]);
        });
    }

    #[test]
    fn union_and_subtract_match_a_bitset() {
        frame::task(Tag::Mir, |frame| {
            // Every pair of small sets, against the points they hold.
            let shapes: Vec<Vec<usize>> = vec![
                vec![],
                vec![0],
                vec![0, 1, 2],
                vec![3, 4, 7],
                vec![1, 5, 6, 7, 12],
                vec![0, 2, 4, 6, 8, 10],
                vec![9, 10, 11, 12],
            ];
            for a in &shapes {
                for b in &shapes {
                    let mut u = set(frame, a);
                    let changed = u.union(&set(frame, b));
                    let mut want: Vec<usize> = a.iter().chain(b).copied().collect();
                    want.sort();
                    want.dedup();
                    assert_eq!(u.iter().collect::<Vec<_>>(), want, "{a:?} | {b:?}");
                    assert_eq!(u, set(frame, &want));
                    assert_eq!(changed, want.len() != a.len());
                    let mut d = set(frame, a);
                    d.subtract(&set(frame, b));
                    let want: Vec<usize> =
                        a.iter().copied().filter(|it| !b.contains(it)).collect();
                    assert_eq!(d.iter().collect::<Vec<_>>(), want, "{a:?} - {b:?}");
                    assert_eq!(d, set(frame, &want));
                    for start in 0..13 {
                        for end in start..13 {
                            let want = (start..=end).find(|it| !a.contains(it));
                            assert_eq!(set(frame, a).first_absent_in(start, end), want);
                            let runs: Vec<usize> = set(frame, a)
                                .runs_in(start, end)
                                .flat_map(|(s, e)| s..=e)
                                .collect();
                            let want: Vec<usize> =
                                (start..=end).filter(|it| a.contains(it)).collect();
                            assert_eq!(runs, want);
                            let mut grown = set(frame, a);
                            let mut runs = Vec::new_in(frame);
                            grown.insert_new(start, end, &mut runs);
                            let new: Vec<usize> = runs.iter().flat_map(|&(s, e)| s..=e).collect();
                            let want: Vec<usize> =
                                (start..=end).filter(|it| !a.contains(it)).collect();
                            assert_eq!(new, want);
                            assert!((start..=end).all(|it| grown.contains(it)));
                        }
                    }
                }
            }
        });
    }

    #[test]
    fn merging_reuses_the_sets_room() {
        blaze_mem::frame::task(blaze_mem::Tag::Mir, |frame| {
            let mut a = set(frame, &[0, 2, 4, 6, 8, 10]);
            let b = set(frame, &[1, 5, 6, 7, 12]);
            a.union(&b);
            a.subtract(&b);
            blaze_alloc_count::assert_at_most(0, "a union and a subtraction", || {
                let used = frame.used();
                a.union(&b);
                a.subtract(&b);
                assert!(frame.used() <= used + 64);
            });
        });
    }
}
