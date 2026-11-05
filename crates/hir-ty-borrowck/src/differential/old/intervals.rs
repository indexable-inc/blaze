//! A set of points as sorted ranges, as rustc's `IntervalSet`: what a region holds is a few
//! runs of consecutive points (a local is live from where it is assigned to its last use), so
//! a region costs its runs, not the body's size.

use smallvec::SmallVec;

/// Sorted, disjoint, non-adjacent inclusive ranges of points.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub(crate) struct IntervalSet {
    ranges: SmallVec<[(u32, u32); 2]>,
}

impl IntervalSet {
    pub(crate) fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// The index of the first range that ends at or after `point`.
    fn first_ending_at_or_after(&self, point: u32) -> usize {
        self.ranges.partition_point(|&(_, end)| end < point)
    }

    pub(crate) fn contains(&self, point: usize) -> bool {
        let point = point as u32;
        let at = self.first_ending_at_or_after(point);
        self.ranges.get(at).is_some_and(|&(start, _)| start <= point)
    }

    pub(crate) fn insert(&mut self, point: usize) -> bool {
        self.insert_range(point, point)
    }

    /// Adds the points `start..=end`, and says whether that added any.
    pub(crate) fn insert_range(&mut self, start: usize, end: usize) -> bool {
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
    pub(crate) fn union(&mut self, other: &IntervalSet) -> bool {
        if other.ranges.is_empty() {
            return false;
        }
        if self.ranges.is_empty() {
            self.ranges = other.ranges.clone();
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
        let mut out: SmallVec<[(u32, u32); 2]> =
            SmallVec::with_capacity(self.ranges.len() + other.ranges.len());
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
                Some(last) if next.0 <= last.1.saturating_add(1) => last.1 = last.1.max(next.1),
                _ => out.push(next),
            }
        }
        let changed = out != self.ranges;
        self.ranges = out;
        changed
    }

    /// Removes the points of `other`.
    pub(crate) fn subtract(&mut self, other: &IntervalSet) {
        if self.ranges.is_empty() || other.ranges.is_empty() {
            return;
        }
        let mut out: SmallVec<[(u32, u32); 2]> = SmallVec::new();
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
                    out.push((start, cs - 1));
                }
                if ce >= end {
                    left = false;
                    break;
                }
                start = ce + 1;
                cut.next();
            }
            if left {
                out.push((start, end));
            }
        }
        self.ranges = out;
    }

    /// The first point of `start..=end` not in the set, if there is one.
    pub(crate) fn first_absent_in(&self, start: usize, end: usize) -> Option<usize> {
        let (start, end) = (start as u32, end as u32);
        let at = self.first_ending_at_or_after(start);
        match self.ranges.get(at) {
            Some(&(s, e)) if s <= start => (e < end).then(|| e as usize + 1),
            _ => Some(start as usize),
        }
    }

    /// The runs of `start..=end` in the set, clipped to it.
    pub(crate) fn runs_in(
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

    /// Adds `start..=end`, and gives the runs of it that were not in the set before.
    pub(crate) fn insert_new(&mut self, start: usize, end: usize) -> SmallVec<[(usize, usize); 2]> {
        let mut new = SmallVec::new();
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
        new
    }

    pub(crate) fn clear(&mut self) {
        self.ranges.clear();
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.ranges.iter().flat_map(|&(start, end)| start as usize..=end as usize)
    }
}
