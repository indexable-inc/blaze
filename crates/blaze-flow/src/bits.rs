//! Rows of bits in one flat array: the one set structure of every dataflow here.
#![blaze::sans_io]

use blaze_own::Vec;
use core::alloc::Allocator;

/// `rows` rows of `cols` bits each, row-major, `cols.div_ceil(64)` words a row.
pub struct Bits<A: Allocator> {
    words: usize,
    data: Vec<u64, A>,
}

impl<A: Allocator> Bits<A> {
    pub fn new_in(rows: usize, cols: usize, alloc: A) -> Bits<A> {
        let words = cols.div_ceil(64);
        let mut data = Vec::with_capacity_in(rows * words, alloc);
        data.resize(rows * words, 0);
        Bits { words, data }
    }

    pub fn words(&self) -> usize {
        self.words
    }

    pub fn row(&self, r: usize) -> &[u64] {
        &self.data[r * self.words..(r + 1) * self.words]
    }

    pub fn row_mut(&mut self, r: usize) -> &mut [u64] {
        &mut self.data[r * self.words..(r + 1) * self.words]
    }

    pub fn get(&self, r: usize, c: usize) -> bool {
        self.data[r * self.words + c / 64] >> (c % 64) & 1 == 1
    }

    /// Sets the bit; says whether it was clear.
    pub fn set(&mut self, r: usize, c: usize) -> bool {
        let w = &mut self.data[r * self.words + c / 64];
        let was = *w;
        *w |= 1 << (c % 64);
        *w != was
    }

    pub fn clear(&mut self, r: usize, c: usize) {
        self.data[r * self.words + c / 64] &= !(1 << (c % 64));
    }

    /// Row `dst` |= row `src`; says whether `dst` grew.
    pub fn union_rows(&mut self, dst: usize, src: usize) -> bool {
        let mut grew = false;
        for i in 0..self.words {
            let s = self.data[src * self.words + i];
            let d = &mut self.data[dst * self.words + i];
            let was = *d;
            *d |= s;
            grew |= *d != was;
        }
        grew
    }
}

/// `dst |= src`; says whether `dst` grew.
pub fn union_into(dst: &mut [u64], src: &[u64]) -> bool {
    let mut grew = false;
    for (d, s) in dst.iter_mut().zip(src) {
        let was = *d;
        *d |= *s;
        grew |= *d != was;
    }
    grew
}

/// `dst &= src`; says whether `dst` shrank.
pub fn intersect_into(dst: &mut [u64], src: &[u64]) -> bool {
    let mut shrank = false;
    for (d, s) in dst.iter_mut().zip(src) {
        let was = *d;
        *d &= *s;
        shrank |= *d != was;
    }
    shrank
}

pub fn test(row: &[u64], c: usize) -> bool {
    row[c / 64] >> (c % 64) & 1 == 1
}

pub fn put(row: &mut [u64], c: usize, on: bool) {
    if on {
        row[c / 64] |= 1 << (c % 64);
    } else {
        row[c / 64] &= !(1 << (c % 64));
    }
}
