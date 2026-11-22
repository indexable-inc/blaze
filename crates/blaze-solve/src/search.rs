//! The search graph's tables (rustc_type_ir search_graph/mod.rs, global_cache.rs, and
//! rustc_next_trait_solver solve/search_graph.rs, the delegate): canonical inputs interned to
//! ids, the global cache per input, the provisional cache, and the path kinds and cycle heads
//! the stack entries carry. The stack itself is the engine's frames (eval.rs).

use blaze_hash::{FixedState, Map, MapOpts};
use blaze_own::{Alloc, Vec};

use crate::canon::RespId;
use crate::error::SolveError;

/// The kind of a path from a cycle head to a goal (`PathKind`), ordered so that `extend` is
/// the maximum: any coinductive step makes the path coinductive, any forced-ambiguity step
/// forces ambiguity (`PathKind::extend`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum PathKind {
    Inductive = 0,
    Unknown = 1,
    Coinductive = 2,
    ForcedAmbiguity = 3,
}

impl PathKind {
    /// `PathKind::extend`.
    pub fn extend(self, rest: PathKind) -> PathKind {
        if self >= rest { self } else { rest }
    }

    pub fn bit(self) -> u8 {
        1 << (self as u8)
    }

    pub const ALL: [PathKind; 4] = [PathKind::Inductive, PathKind::Unknown, PathKind::Coinductive, PathKind::ForcedAmbiguity];
}

/// How many times a cycle head is rerun before its result is overflow
/// (`FIXPOINT_STEP_LIMIT`).
pub const FIXPOINT_STEP_LIMIT: u32 = 8;

/// The deepest stack the solver keeps: the cycle heads are a bitset over stack indices (rule
/// 18). A recursion limit above it is clamped to it.
pub const MAX_DEPTH: u32 = 512;

/// A set of stack indices: the cycle heads an entry depends on (`CycleHeads`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Heads(pub [u64; (MAX_DEPTH / 64) as usize]);

impl Heads {
    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|&w| w == 0)
    }

    pub fn insert(&mut self, i: u32) {
        self.0[(i / 64) as usize] |= 1 << (i % 64);
    }

    pub fn remove(&mut self, i: u32) {
        self.0[(i / 64) as usize] &= !(1 << (i % 64));
    }

    pub fn union(&mut self, o: &Heads) {
        for (a, b) in self.0.iter_mut().zip(o.0.iter()) {
            *a |= *b;
        }
    }

    /// The highest index in the set.
    pub fn highest(&self) -> Option<u32> {
        for (w, &bits) in self.0.iter().enumerate().rev() {
            if bits != 0 {
                return Some(w as u32 * 64 + 63 - bits.leading_zeros());
            }
        }
        None
    }

    /// The set without every index at or above `n` (heads deeper than a stack of `n`).
    pub fn below(&self, n: u32) -> Heads {
        let mut h = *self;
        for i in 0..MAX_DEPTH / 64 {
            let lo = i * 64;
            if lo >= n {
                h.0[i as usize] = 0;
            } else if lo + 64 > n {
                h.0[i as usize] &= (1u64 << (n - lo)) - 1;
            }
        }
        h
    }
}

/// An answer: a response, or no solution.
pub type Ans = Option<RespId>;

/// A global cache entry (`global_cache.rs` `Success`).
#[derive(Clone, Copy, Debug)]
pub struct Entry {
    pub ans: Ans,
    pub required_depth: u32,
    /// The nested goals (input ids) below this one whose answer may change if they are moved
    /// onto the stack: the entry does not apply while one of them is on the stack
    /// (`candidate_is_applicable`). As the reference's `NestedGoals`, only the goals that met a
    /// cycle (the heads met, and every goal between) or overflow are kept, never a cycle-free
    /// subtree's (eval.rs `finish`, "WHICH NESTED GOALS"). `u32::MAX` length: too many to keep,
    /// the entry applies only with an empty stack.
    pub nested: (u32, u32),
    pub reads: (u32, u32),
    /// Computed with overflow (an [`OverflowEntry`]'s): its parent keeps it as a nested goal.
    pub overflow: bool,
}

/// A global cache entry computed with overflow, valid only at the depth it was computed at
/// (`global_cache.rs` `WithOverflow`).
#[derive(Clone, Copy, Debug)]
pub struct OverflowEntry {
    pub input: u32,
    pub depth: u32,
    pub entry: Entry,
}

/// A provisional cache entry (`ProvisionalCacheEntry`): the result of a goal that depends on
/// cycle heads still on the stack, valid while they are and the path from the highest head
/// to the goal is the same kind.
#[derive(Clone, Copy, Debug)]
pub struct Prov {
    pub input: u32,
    pub heads: Heads,
    pub highest: u32,
    pub path: PathKind,
    pub ans: Ans,
    /// Whether its answer met overflow (rustc `ProvisionalCacheEntry::encountered_overflow`):
    /// a hit marks the goal that took it, as a global cache hit does.
    pub overflow: bool,
}

/// The most nested-goal ids a cache entry keeps (rule 18); past it, see `Entry::nested`.
pub const MAX_NESTED: usize = 4096;

/// The canonical inputs, interned, with their caches.
pub struct Inputs<'a> {
    pub words: Vec<u32, Alloc<'a>>,
    rows: Vec<(u32, u32), Alloc<'a>>,
    /// Hash of the words to the first row with it; `next` chains rows of equal hash.
    map: Map<u64, u32, FixedState, Alloc<'a>>,
    next: Vec<u32, Alloc<'a>>,
    /// Per input: its global cache entry.
    pub success: Vec<Option<Entry>, Alloc<'a>>,
    pub overflow: Vec<OverflowEntry, Alloc<'a>>,
    /// Per input: its stack index, or `u32::MAX`.
    pub on_stack: Vec<u32, Alloc<'a>>,
    pub nested_pool: Vec<u32, Alloc<'a>>,
    /// The entries' read lists, as the solver's read ids (`readlog::ReadLog`).
    pub reads_pool: Vec<u32, Alloc<'a>>,
    pub prov: Vec<Prov, Alloc<'a>>,
    /// Per input: head index into `prov` (or `NONE`), chained via `prov_next` in FIFO order so
    /// provisional lookup only inspects entries belonging to `input`.
    prov_head: Vec<u32, Alloc<'a>>,
    prov_next: Vec<u32, Alloc<'a>>,
    /// Maximum `p.highest` across all entries currently in `prov` (0 when `prov` is empty),
    /// enabling O(1) skip in `drop_provisional` when no entry depends on `>= head`.
    prov_max_highest: u32,
}

const NONE: u32 = u32::MAX;

/// FNV-1a over the words, then a final mix: the canonical key's hash (deterministic: rule 25).
pub fn hash_words(w: &[u32]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &x in w {
        h = (h ^ u64::from(x)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h ^= h >> 31;
    h = h.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    h ^ (h >> 29)
}

impl<'a> Inputs<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Inputs<'a> {
        Inputs {
            words: Vec::new_in(alloc),
            rows: Vec::new_in(alloc),
            map: Map::new(MapOpts { hasher: FixedState, alloc: alloc, capacity: None }),
            next: Vec::new_in(alloc),
            success: Vec::new_in(alloc),
            overflow: Vec::new_in(alloc),
            on_stack: Vec::new_in(alloc),
            nested_pool: Vec::new_in(alloc),
            reads_pool: Vec::new_in(alloc),
            prov: Vec::new_in(alloc),
            prov_head: Vec::new_in(alloc),
            prov_next: Vec::new_in(alloc),
            prov_max_highest: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn words_of(&self, id: u32) -> &[u32] {
        let (s, l) = self.rows[id as usize];
        &self.words[s as usize..(s + l) as usize]
    }

    /// The id of the input `w`, interning it.
    pub fn intern(&mut self, w: &[u32]) -> Result<u32, SolveError> {
        let h = hash_words(w);
        let mut at = self.map.get(&h).copied().unwrap_or(NONE);
        while at != NONE {
            if self.words_of(at) == w {
                return Ok(at);
            }
            at = self.next[at as usize];
        }
        let id = crate::error::row(self.rows.len())?;
        let start = crate::error::row(self.words.len())?;
        self.words.extend_from_slice(w);
        self.rows.push((start, w.len() as u32));
        let head = self.map.insert(h, id).unwrap_or(NONE);
        self.next.push(head);
        self.success.push(None);
        self.on_stack.push(NONE);
        self.prov_head.push(NONE);
        Ok(id)
    }

    /// The global cache's answer for `input` at `depth` available, if applicable
    /// (`GlobalCache::get` with `candidate_is_applicable`).
    pub fn lookup(&self, input: u32, depth: u32, stack_empty: bool) -> Option<Entry> {
        let applicable = |e: &Entry| -> bool {
            if e.nested.1 == u32::MAX {
                return stack_empty;
            }
            let (s, l) = e.nested;
            self.nested_pool[s as usize..(s + l) as usize].iter().all(|&n| self.on_stack[n as usize] == NONE)
        };
        if let Some(e) = self.success[input as usize]
            && depth >= e.required_depth
            && applicable(&e)
        {
            return Some(e);
        }
        if !self.overflow.is_empty() {
            return self
                .overflow
                .iter()
                .find(|o| o.input == input && o.depth == depth && applicable(&o.entry))
                .map(|o| o.entry);
        }
        None
    }

    /// Stores a finished goal's answer: `nested` its nested goals and `reads` its read ids,
    /// each a set in first-seen order (`readlog::SegLog`).
    pub fn insert(
        &mut self,
        input: u32,
        depth: u32,
        overflow: bool,
        ans: Ans,
        required_depth: u32,
        nested: &[u32],
        big: bool,
        reads: &[u32],
    ) -> Result<Entry, SolveError> {
        let nested_range = if big || nested.len() > MAX_NESTED {
            (0, u32::MAX)
        } else {
            let s = crate::error::row(self.nested_pool.len())?;
            self.nested_pool.extend_from_slice(nested);
            (s, nested.len() as u32)
        };
        let rs = crate::error::row(self.reads_pool.len())?;
        self.reads_pool.extend_from_slice(reads);
        let entry = Entry { ans, required_depth, nested: nested_range, reads: (rs, reads.len() as u32), overflow };
        if overflow {
            self.overflow.push(OverflowEntry { input, depth, entry });
        } else {
            self.success[input as usize] = Some(entry);
        }
        Ok(entry)
    }

    pub fn reads_of(&self, e: &Entry) -> &[u32] {
        &self.reads_pool[e.reads.0 as usize..(e.reads.0 + e.reads.1) as usize]
    }

    pub fn nested_of(&self, e: &Entry) -> Option<&[u32]> {
        if e.nested.1 == u32::MAX {
            return None;
        }
        Some(&self.nested_pool[e.nested.0 as usize..(e.nested.0 + e.nested.1) as usize])
    }

    /// First provisional entry index for `input`, or `u32::MAX` when none exist.
    #[inline]
    pub fn prov_head(&self, input: u32) -> u32 {
        self.prov_head[input as usize]
    }

    /// Next provisional entry index in the same input's chain, or `u32::MAX`.
    #[inline]
    pub fn prov_next(&self, idx: u32) -> u32 {
        self.prov_next[idx as usize]
    }

    /// Appends a provisional entry and links it at the tail of `p.input`'s chain (preserving
    /// exact FIFO lookup order).
    pub fn push_provisional(&mut self, p: Prov) {
        let idx = self.prov.len() as u32;
        if self.prov.is_empty() || p.highest > self.prov_max_highest {
            self.prov_max_highest = p.highest;
        }
        self.prov.push(p);
        self.prov_next.push(NONE);
        let inp = p.input as usize;
        let head = self.prov_head[inp];
        if head == NONE {
            self.prov_head[inp] = idx;
        } else {
            let mut cur = head;
            while self.prov_next[cur as usize] != NONE {
                cur = self.prov_next[cur as usize];
            }
            self.prov_next[cur as usize] = idx;
        }
    }

    /// Clears all provisional entries and unlinks their per-input heads.
    pub fn clear_provisional(&mut self) {
        for p in self.prov.iter() {
            self.prov_head[p.input as usize] = NONE;
        }
        self.prov.clear();
        self.prov_next.clear();
        self.prov_max_highest = 0;
    }

    /// Drops the provisional entries that depend on the stack entry `head` (the reference
    /// rebases them when a head finishes; solve.md, "Not ported").
    pub fn drop_provisional(&mut self, head: u32) {
        if self.prov.is_empty() || self.prov_max_highest < head {
            return;
        }
        for p in self.prov.iter() {
            self.prov_head[p.input as usize] = NONE;
        }
        let mut write = 0usize;
        let mut max_h = 0u32;
        for read in 0..self.prov.len() {
            let p = self.prov[read];
            if p.highest < head {
                self.prov[write] = p;
                if write == 0 || p.highest > max_h {
                    max_h = p.highest;
                }
                write += 1;
            }
        }
        self.prov.truncate(write);
        self.prov_next.truncate(write);
        self.prov_max_highest = max_h;
        // Relink in reverse so each input's chain stays in exact FIFO order (`0..write`).
        for idx in (0..write).rev() {
            let inp = self.prov[idx].input as usize;
            self.prov_next[idx] = self.prov_head[inp];
            self.prov_head[inp] = idx as u32;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provisional_chain_fifo_drop_and_clear() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut inputs = Inputs::new_in(alloc);
        let a = inputs.intern(&[1]).unwrap();
        let b = inputs.intern(&[2]).unwrap();
        let c = inputs.intern(&[3]).unwrap();

        let mk = |input: u32, highest: u32, path: PathKind| Prov {
            input,
            heads: Heads::default(),
            highest,
            path,
            ans: None,
            overflow: false,
        };
        inputs.push_provisional(mk(a, 1, PathKind::Coinductive));
        inputs.push_provisional(mk(b, 3, PathKind::Unknown));
        inputs.push_provisional(mk(a, 2, PathKind::ForcedAmbiguity));
        inputs.push_provisional(mk(a, 3, PathKind::Inductive));

        let chain = |inp: &Inputs<'_>, id: u32| -> std::vec::Vec<(u32, PathKind)> {
            let mut out = std::vec::Vec::new();
            let mut at = inp.prov_head(id);
            while at != NONE {
                let p = inp.prov[at as usize];
                assert_eq!(p.input, id);
                out.push((p.highest, p.path));
                at = inp.prov_next(at);
            }
            out
        };

        assert_eq!(chain(&inputs, a), [(1, PathKind::Coinductive), (2, PathKind::ForcedAmbiguity), (3, PathKind::Inductive)]);
        assert_eq!(chain(&inputs, b), [(3, PathKind::Unknown)]);
        assert_eq!(chain(&inputs, c), []);

        // No-op drop above max highest.
        inputs.drop_provisional(4);
        assert_eq!(inputs.prov.len(), 4);

        // Drop highest >= 3: b disappears, a keeps (1, 2) in FIFO order.
        inputs.drop_provisional(3);
        assert_eq!(chain(&inputs, a), [(1, PathKind::Coinductive), (2, PathKind::ForcedAmbiguity)]);
        assert_eq!(chain(&inputs, b), []);

        // Clear unlinks everything cleanly.
        inputs.clear_provisional();
        assert!(inputs.prov.is_empty());
        assert_eq!(inputs.prov_head(a), NONE);
        assert_eq!(inputs.prov_head(b), NONE);
    }
}
