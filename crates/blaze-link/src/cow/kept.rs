//! [`CowVec`], [`CowList`] and [`CowBytes`] as parts of a kept state ([`crate::keep`]).
//!
//! A column is its elements. A list arena is its chunks exactly as they are (each chunk's
//! class, the elements up to the run of copies its tail is, and that copy), with its counters
//! and its garbage, so every [`super::ListRef`] into it points where it did and compaction is
//! due when it was. A region's bytes are written up to their last byte that is not room and
//! read back in place ([`CowBytes::kept`]).

#![forbid(unsafe_code)]

use std::alloc::{Allocator, Global};

use crate::keep::{Bad, In, Kept, Out};

use super::{CLASSES, Chunk, Chunked, CowBytes, CowList, CowVec, ENTRIES, LONGEST_BYTES};

impl<T: Kept + Chunked> Kept for CowVec<T> {
    fn put<A: Allocator>(&self, out: &mut Out<A>) {
        out.len(self.len());
        for it in self.iter() {
            it.put(out);
        }
    }

    fn take(input: &mut In<'_>) -> Result<CowVec<T>, Bad> {
        let count = input.count(1)?;
        let mut column = CowVec::default();
        for _ in 0..count {
            column.push(T::take(input)?);
        }
        Ok(column)
    }
}

impl<T: Kept + Chunked + PartialEq> Kept for CowList<T> {
    fn put<A: Allocator>(&self, out: &mut Out<A>) {
        let CowList { chunks, count, fill, open, live, garbage } = self;
        out.len(*count);
        out.len(*fill);
        out.bool(*open);
        out.len(*live);
        out.len(*garbage);
        for at in 0..*count {
            let items = chunks.get(at).expect("a chunk below the arena's count").items();
            let last = items.last().expect("a chunk holds at least one element");
            let head = items.iter().rposition(|it| it != last).map_or(0, |it| it + 1);
            out.len(items.len());
            out.len(head);
            for it in &items[..head] {
                it.put(out);
            }
            last.put(out);
        }
    }

    fn take(input: &mut In<'_>) -> Result<CowList<T>, Bad> {
        let count = input.count(16)?;
        let (fill, open) = (input.len()?, input.bool()?);
        let (live, garbage) = (input.len()?, input.len()?);
        if count > ENTRIES {
            return Err(Bad::What("a list arena of more chunks than its table indexes"));
        }
        let mut list = CowList::default();
        let mut items = Vec::new_in(Global);
        let mut last_len = 0;
        for at in 0..count {
            let len = input.len()?;
            if !CLASSES.contains(&len) {
                return Err(Bad::What("a list arena's chunk of a length no class has"));
            }
            let head = input.count(1)?;
            if head > len {
                return Err(Bad::What("a list arena's chunk holding more than its class"));
            }
            items.clear();
            for _ in 0..head {
                items.push(T::take(input)?);
            }
            let tail = T::take(input)?;
            *list.chunks.entry_mut(at) = Some(Chunk::new(len, &items, &tail));
            last_len = len;
        }
        if (count > 0 && fill > last_len) || (open && last_len != super::CHUNK) {
            return Err(Bad::What("a list arena whose last chunk is not as its counters say"));
        }
        list.count = count;
        list.fill = fill;
        list.open = open;
        list.live = live;
        list.garbage = garbage;
        Ok(list)
    }
}

impl Kept for CowBytes {
    fn put<A: Allocator>(&self, out: &mut Out<A>) {
        let held = self.held();
        out.len(self.len);
        out.len(held);
        out.align();
        let mut end = 0;
        self.runs(|at, run| {
            out.zeros(at - end);
            out.raw(run);
            end = at + run.len();
        });
        out.zeros(held - end);
        out.align();
    }

    fn take(input: &mut In<'_>) -> Result<CowBytes, Bad> {
        let (len, held) = (input.len()?, input.len()?);
        if held > len || len > LONGEST_BYTES {
            return Err(Bad::What("a region whose bytes are longer than it, or than a region can be"));
        }
        input.align()?;
        let (map, at) = input.in_place(held)?;
        input.align()?;
        Ok(CowBytes::kept(map, at, held, len))
    }
}
