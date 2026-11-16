//! A stack of open lists and maps, one bit each: the first [`INLINE`] levels in place, so the
//! usual text allocates nothing for nesting, and the rest in the caller's allocator, so any
//! depth limit is held without recursion and in memory linear in the depth (a bit a level).

use core::alloc::Allocator;

use blaze_own::Vec;

/// The levels held in place before the stack spills into its allocator.
pub(crate) const INLINE: usize = 1024;

const WORDS: usize = INLINE / 64;

pub(crate) struct Stack<S: Allocator> {
    /// Bit set for a map, clear for a list.
    inline: [u64; WORDS],
    /// Levels from [`INLINE`] up, 64 a word.
    spill: Vec<u64, S>,
    depth: usize,
}

impl<S: Allocator> Stack<S> {
    pub(crate) fn new_in(scratch: S) -> Stack<S> {
        Stack { inline: [0; WORDS], spill: Vec::new_in(scratch), depth: 0 }
    }

    pub(crate) fn depth(&self) -> usize {
        self.depth
    }

    /// Pushes; the caller has checked the depth against its limit.
    pub(crate) fn push(&mut self, map: bool) {
        let (word, bit) = (self.depth / 64, self.depth % 64);
        let slot = match word.checked_sub(WORDS) {
            None => &mut self.inline[word],
            Some(spilled) => {
                if spilled == self.spill.len() {
                    self.spill.push(0);
                }
                &mut self.spill[spilled]
            }
        };
        match map {
            true => *slot |= 1 << bit,
            false => *slot &= !(1 << bit),
        }
        self.depth += 1;
    }

    pub(crate) fn pop(&mut self) {
        self.depth -= 1;
    }

    /// `Some(true)` inside a map, `Some(false)` inside a list, `None` at the top.
    pub(crate) fn top(&self) -> Option<bool> {
        let i = self.depth.checked_sub(1)?;
        let (word, bit) = (i / 64, i % 64);
        let bits = match word.checked_sub(WORDS) {
            None => self.inline[word],
            Some(spilled) => self.spill[spilled],
        };
        Some(bits & (1 << bit) != 0)
    }
}
