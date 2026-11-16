//! [`Window`]: the newest items of one stream, by sequence number, in a fixed amount of memory:
//! one byte ring of `bytes` and an index of at most `items` entries, both allocated once. A
//! push drops the oldest items until the new one fits; a reader names the sequence number it
//! wants and learns from [`Window::first`] whether the window still holds it. Nothing waits:
//! the writer never waits for a reader, and a reader that fell behind gets a gap.

// adapted from makepad/relax (MIT), libs/remote/src/ring.rs (the replay ring: one fixed buffer,
// the oldest dropped to make room, a sequence number a record, readers that keep their own
// place and learn what was lost); changed: no lock (one owner, a sans-IO machine), the index
// beside the bytes instead of a header in them, and a refusal of an item larger than the ring.

#![blaze::sans_io]

use core::alloc::Allocator;
use core::fmt;

use blaze_own::{Vec, VecDeque};

use crate::Seq;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Span {
    at: u64,
    len: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WindowError {
    /// A window of no bytes or no items.
    Empty,
    /// An item larger than the whole window.
    TooLarge { len: usize, bytes: usize },
    /// A sequence number the window does not hold.
    Gone { seq: Seq, first: Seq, next: Seq },
}

impl fmt::Display for WindowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            WindowError::Empty => write!(f, "a replay window of no bytes or no items"),
            WindowError::TooLarge { len, bytes } => {
                write!(f, "an item of {len} bytes for a replay window of {bytes}")
            }
            WindowError::Gone { seq, first, next } => write!(
                f,
                "item {} is not in the replay window, which holds {} to {}",
                seq.0,
                first.0,
                next.0.saturating_sub(1)
            ),
        }
    }
}

impl core::error::Error for WindowError {}

pub struct Window<A: Allocator> {
    ring: Vec<u8, A>,
    spans: VecDeque<Span, A>,
    most_items: usize,
    /// Where the oldest held byte is, counted from the first byte ever pushed.
    start: u64,
    end: u64,
    /// The sequence number of the oldest item held (`next` when none is).
    first: Seq,
    next: Seq,
}

impl<A: Allocator + Clone> Window<A> {
    pub fn new_in(bytes: usize, items: usize, alloc: A) -> Result<Window<A>, WindowError> {
        if bytes == 0 || items == 0 {
            return Err(WindowError::Empty);
        }
        let mut ring = Vec::with_capacity_in(bytes, alloc.clone());
        ring.resize(bytes, 0);
        Ok(Window {
            ring,
            spans: VecDeque::with_capacity_in(items, alloc),
            most_items: items,
            start: 0,
            end: 0,
            first: Seq(1),
            next: Seq(1),
        })
    }
}

impl<A: Allocator> Window<A> {
    /// The oldest item held; equal to [`Window::next`] when the window is empty.
    pub fn first(&self) -> Seq {
        self.first
    }

    /// The number the next push gets.
    pub fn next(&self) -> Seq {
        self.next
    }

    /// The newest item's number, or [`Seq::NONE`] before the first push.
    pub fn newest(&self) -> Seq {
        Seq(self.next.0 - 1)
    }

    pub fn len(&self) -> usize {
        self.spans.len()
    }

    /// The memory the window holds, fixed when it was made.
    pub fn footprint(&self) -> usize {
        self.ring.len() + self.most_items * size_of::<Span>()
    }

    fn drop_oldest(&mut self) {
        if self.spans.pop_front().is_some() {
            self.first = self.first.next();
            self.start = self.spans.front().map_or(self.end, |span| span.at);
        }
    }

    /// Pushes one item, dropping the oldest until it fits; returns its number.
    pub fn push(&mut self, item: &[u8]) -> Result<Seq, WindowError> {
        let bytes = self.ring.len();
        if item.len() > bytes {
            return Err(WindowError::TooLarge { len: item.len(), bytes });
        }
        while self.spans.len() == self.most_items || (self.end - self.start) as usize + item.len() > bytes {
            self.drop_oldest();
        }
        let at = (self.end % bytes as u64) as usize;
        let first = item.len().min(bytes - at);
        self.ring[at..at + first].copy_from_slice(&item[..first]);
        self.ring[..item.len() - first].copy_from_slice(&item[first..]);
        self.spans.push_back(Span { at: self.end, len: item.len() as u32 });
        self.end += item.len() as u64;
        let seq = self.next;
        self.next = seq.next();
        Ok(seq)
    }

    /// Item `seq`'s bytes, as the two parts the ring holds them in (the second empty unless it
    /// wraps).
    pub fn get(&self, seq: Seq) -> Result<(&[u8], &[u8]), WindowError> {
        if seq < self.first || seq >= self.next {
            return Err(WindowError::Gone { seq, first: self.first, next: self.next });
        }
        let span = self.spans[(seq.0 - self.first.0) as usize];
        let bytes = self.ring.len();
        let at = (span.at % bytes as u64) as usize;
        let len = span.len as usize;
        let first = len.min(bytes - at);
        Ok((&self.ring[at..at + first], &self.ring[..len - first]))
    }

    /// Item `seq`'s length.
    pub fn len_of(&self, seq: Seq) -> Result<usize, WindowError> {
        self.get(seq).map(|(a, b)| a.len() + b.len())
    }

    /// Appends item `seq`'s bytes to `out`.
    pub fn copy_into<B: Allocator>(&self, seq: Seq, out: &mut Vec<u8, B>) -> Result<(), WindowError> {
        let (a, b) = self.get(seq)?;
        out.extend_from_slice(a);
        out.extend_from_slice(b);
        Ok(())
    }
}
