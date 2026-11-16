//! A stack of open lists and maps, one bit each, in place: the reader and the writer allocate
//! nothing for nesting.

/// The most lists and maps a reader or writer holds open at once.
pub const MOST_DEPTH: usize = 1024;

const WORDS: usize = MOST_DEPTH / 64;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Stack {
    /// Bit set for a map, clear for a list.
    bits: [u64; WORDS],
    depth: usize,
}

impl Stack {
    pub(crate) const fn new() -> Stack {
        Stack { bits: [0; WORDS], depth: 0 }
    }

    pub(crate) fn depth(&self) -> usize {
        self.depth
    }

    /// Pushes; the caller has checked the depth against its limit, at most [`MOST_DEPTH`].
    pub(crate) fn push(&mut self, map: bool) {
        let (word, bit) = (self.depth / 64, self.depth % 64);
        match map {
            true => self.bits[word] |= 1 << bit,
            false => self.bits[word] &= !(1 << bit),
        }
        self.depth += 1;
    }

    pub(crate) fn pop(&mut self) {
        self.depth -= 1;
    }

    /// `Some(true)` inside a map, `Some(false)` inside a list, `None` at the top.
    pub(crate) fn top(&self) -> Option<bool> {
        let i = self.depth.checked_sub(1)?;
        Some(self.bits[i / 64] & (1 << (i % 64)) != 0)
    }
}
