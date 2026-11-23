//! The point CFG and the solver's bounded, duplicate-free worklist. Facts are seeded
//! even in disconnected blocks, so reverse postorder covers the whole DFS forest.
#![blaze::sans_io]

use blaze_own::Vec;
use core::alloc::Allocator;

use super::solver::{Exhausted, Meter};
use crate::flow::{Flow, Point, Stmt};

pub(super) struct Points<A: Allocator> {
    offsets: Vec<usize, A>,
    edges: Vec<usize, A>,
    postorder: Vec<usize, A>,
    cycles: Option<Ready<A>>,
}

/// On cyclic CFGs the smallest RPO rank runs first, including on revisits.
/// A bit in an upper level names a nonempty word below. A u32 point needs
/// at most six word operations to locate, independent of the distance between
/// the changed points. No ready entry can occur twice.
struct Ready<A: Allocator> {
    rank: Vec<usize, A>,
    // At rank r, largest exclusive end of a back-edge interval starting at or before r.
    back_until: Vec<usize, A>,
    levels: Vec<usize, A>,
    bits: Vec<u64, A>,
}

impl<A: Allocator + Copy> Ready<A> {
    fn new(postorder: &[usize], offsets: &[usize], edges: &[usize], alloc: A) -> Self {
        let mut rank = Vec::from_elem_in(0, postorder.len(), alloc);
        for (i, &point) in postorder.iter().rev().enumerate() {
            rank[point] = i;
        }
        let mut back_until = Vec::from_elem_in(0, postorder.len(), alloc);
        for (p, &from) in rank.iter().enumerate() {
            for &q in &edges[offsets[p]..offsets[p + 1]] {
                let to = rank[q];
                if to <= from { back_until[to] = back_until[to].max(from + 1); }
            }
        }
        let mut end = 0;
        for interval in &mut back_until {
            end = end.max(*interval);
            *interval = end;
        }
        let mut levels = Vec::new_in(alloc);
        let mut bits = Vec::new_in(alloc);
        let mut count = postorder.len();
        loop {
            let start = bits.len();
            levels.push(start);
            let words = count.div_ceil(64);
            bits.resize(start + words, u64::MAX);
            let tail = count % 64;
            if tail != 0 {
                bits[start + words - 1] = (1u64 << tail) - 1;
            }
            if words == 1 { break; }
            count = words;
        }
        Self { rank, back_until, levels, bits }
    }

    fn pop(&mut self) -> Option<usize> {
        let mut index = 0;
        for &start in self.levels.iter().rev() {
            let word = self.bits[start + index];
            if word == 0 { return None; }
            index = index * 64 + word.trailing_zeros() as usize;
        }
        let rank = index;
        for &start in &self.levels {
            let word = &mut self.bits[start + index / 64];
            *word &= !(1 << (index % 64));
            if *word != 0 { break; }
            index /= 64;
        }
        Some(rank)
    }

    fn schedule(&mut self, point: usize) {
        let mut index = self.rank[point];
        for &start in &self.levels {
            let word = &mut self.bits[start + index / 64];
            let was = *word;
            *word |= 1 << (index % 64);
            if was != 0 { break; }
            index /= 64;
        }
    }
}

impl<A: Allocator + Copy> Points<A> {
    pub(super) fn new(flow: &Flow<A>, meter: &mut Meter, alloc: A) -> Result<Self, Exhausted> {
        let points = 2 * flow.stmts.len();
        // Count and fill the CSR rows, initialise marks, and enter and finish each
        // point once. The edges are charged for counting, filling and DFS separately.
        meter.charge(points.saturating_mul(6))?;
        let mut offsets = Vec::with_capacity_in(points + 1, alloc);
        offsets.push(0usize);
        let mut edge_count = 0;
        for s in 0..flow.stmts.len() {
            let stmt = Stmt(s as u32);
            let before = edge_count;
            edge_count += 1;
            offsets.push(edge_count);
            flow.for_each_succ(stmt, |_| edge_count += 1);
            meter.charge((edge_count - before).saturating_mul(3))?;
            offsets.push(edge_count);
        }
        let mut edges = Vec::with_capacity_in(edge_count, alloc);
        for s in 0..flow.stmts.len() {
            let stmt = Stmt(s as u32);
            edges.push(Point::mid(stmt).index());
            flow.for_each_succ(stmt, |to| edges.push(Point::start(to).index()));
        }

        // An explicit DFS frame holds the next outgoing edge. An edge to a point
        // still on the stack is exactly a cycle, including in disconnected code.
        #[derive(Clone, Copy, PartialEq)]
        enum Mark { New, Visiting, Done }
        let mut marks = Vec::with_capacity_in(points, alloc);
        marks.resize(points, Mark::New);
        let mut stack = Vec::with_capacity_in(points, alloc);
        let mut pending = Vec::with_capacity_in(points, alloc);
        let mut acyclic = true;
        for root in 0..points {
            if marks[root] != Mark::New {
                continue;
            }
            marks[root] = Mark::Visiting;
            stack.push((root, offsets[root]));
            while let Some((point, next)) = stack.last_mut() {
                let p = *point;
                if *next == offsets[p + 1] {
                    marks[p] = Mark::Done;
                    pending.push(p);
                    stack.pop();
                    continue;
                }
                let q = edges[*next];
                *next += 1;
                match marks[q] {
                    Mark::Visiting => acyclic = false,
                    Mark::New => {
                        marks[q] = Mark::Visiting;
                        stack.push((q, offsets[q]));
                    }
                    Mark::Done => {}
                }
            }
        }
        let cycles = if acyclic {
            None
        } else {
            meter.charge(points.saturating_mul(3).saturating_add(edge_count))?;
            Some(Ready::new(&pending, &offsets, &edges, alloc))
        };
        Ok(Self { offsets, edges, postorder: pending, cycles })
    }

    pub(super) fn successors(&self, point: usize) -> &[usize] {
        &self.edges[self.offsets[point]..self.offsets[point + 1]]
    }

    /// Outside all backward-edge intervals, every predecessor has converged when
    /// the point first runs. Any subsequent change would have to cross this rank
    /// backwards, and the scheduler always takes the lowest ready rank first.
    pub(super) fn can_revisit(&self, p: usize) -> bool {
        self.cycles.as_ref().is_some_and(|ready| {
            let rank = ready.rank[p];
            rank < ready.back_until[rank]
        })
    }

    pub(super) fn pop(&mut self) -> Option<usize> {
        if let Some(ready) = &mut self.cycles {
            let rank = ready.pop()?;
            Some(self.postorder[self.postorder.len() - 1 - rank])
        } else {
            self.postorder.pop()
        }
    }

    pub(super) fn schedule(&mut self, point: usize) {
        if let Some(ready) = &mut self.cycles {
            ready.schedule(point);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Ready;
    use std::{alloc::Global, vec::Vec};

    #[test]
    fn changed_points_run_once_in_reverse_postorder_across_word_boundaries() {
        let postorder: Vec<_> = (0..4097).rev().collect();
        let offsets = std::vec![0; postorder.len() + 1];
        let mut ready = Ready::new(&postorder, &offsets, &[], Global);
        for rank in 0..4097 { assert_eq!(ready.pop(), Some(rank)); }
        assert_eq!(ready.pop(), None);
        for point in [4096, 64, 63, 4095, 0, 64, 4096] { ready.schedule(point); }
        for rank in [0, 63, 64, 4095, 4096] { assert_eq!(ready.pop(), Some(rank)); }
        assert_eq!(ready.pop(), None);
    }
}
