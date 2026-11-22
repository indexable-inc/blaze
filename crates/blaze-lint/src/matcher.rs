//! The one matcher every token-sequence rule shares: the sequences (`Alloc :: global (`, `. await`,
//! `todo !`) are one trie over symbols, built by `const fn` as the crate compiles, and a pass keeps
//! the set of trie states still live and advances them all by each token's symbol. A token's
//! symbol is a small number: a word's index in the rules' word table (one perfect-hash probe per
//! identifier, made once while lexing), or one of a few punctuation symbols. So N sequence rules
//! cost one probe per identifier and, per token, one load for the root and one per live state,
//! whatever N is.
//!
//! The trie is dense: `next[node][symbol]`, one [`Node`] each. A state is a node and the token
//! its match started at; two live states never share a start (the trie is deterministic), so
//! with sequences at most `LIVE` symbols long at most `LIVE` states are live after any symbol.
//!
//! **What the table costs.** `nodes x symbols x size_of::<Node>()` bytes, the nodes counted
//! exactly by [`nodes_for`] (one per distinct prefix, the root's included). The rules' table is
//! 292 nodes at two bytes (`Node = u16`) after adding `raw-sync`, `blocking-on-pool`, `transmute`,
//! and strengthened `ambient-hasher`/`ambient-heap` patterns (previously 207 nodes over 126
//! symbols at one byte, 26,082 bytes). (Counted by hand from `sequences::SEQS`; the build
//! checks the count, and `rule_trie_equals_a_scan_per_sequence` the table.)
//!
//! A node number is a byte while the table has at most 254 nodes, and the hot path is fastest so:
//! a token costs one load of the root's row ([`Trie::starts`]) and one per live state, the same
//! loads at any width, but a byte halves every row (121 bytes, two cache lines, against 242 and
//! four) and the whole table, so more of what tokens reach stays in L1. Past 254 nodes the build
//! fails at compile time (`Trie::build`'s assert) and [`Node`] becomes `u16`: every place that
//! stores, compares or indexes a node goes through `Node` and [`DEAD`], so that is the one line
//! to change, and the table then costs `nodes x symbols x 2`.
//!
//! Sans-IO and pure: [`Live::step`] is a function of its state, the trie and one symbol.

/// A node's number: wide enough for every node of the rules' table, with [`DEAD`] above them
/// (`Trie::build` asserts it). `u16` supports up to 65,534 nodes.
pub(crate) type Node = u16;

/// No transition: the symbol ends every match through this node.
pub(crate) const DEAD: Node = Node::MAX;
/// No sequence ends here.
pub(crate) const NO_SEQ: u16 = u16::MAX;
/// The longest sequence, in symbols, and so the most states live at once.
pub(crate) const LIVE: usize = 8;

/// One sequence, as its symbols: what [`Trie::build`] takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Pattern {
    pub(crate) syms: [u8; LIVE],
    pub(crate) len: u8,
}

impl Pattern {
    pub(crate) const EMPTY: Pattern = Pattern { syms: [0; LIVE], len: 0 };

    /// The pattern of `syms`. Panics, a compile error in a `const`, past [`LIVE`] symbols.
    #[cfg(test)]
    pub(crate) const fn of(syms: &[u8]) -> Pattern {
        assert!(!syms.is_empty() && syms.len() <= LIVE, "a sequence of 1 to LIVE symbols");
        let mut out = Pattern::EMPTY;
        let mut at = 0;
        // Bounded: one turn per symbol, LIVE at most.
        while at < syms.len() {
            out.syms[at] = syms[at];
            at += 1;
        }
        out.len = syms.len() as u8;
        out
    }

    #[cfg(test)]
    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.syms[..self.len as usize]
    }
}

/// The nodes the trie of `patterns` has, exactly: the root and one per distinct nonempty prefix
/// of a pattern, which is the node [`Trie::build`] makes when that prefix is first inserted. So a
/// table sized by it has no unused row, and one build is enough.
pub(crate) const fn nodes_for(patterns: &[Pattern]) -> usize {
    let mut nodes = 1;
    let mut k = 0;
    // Bounded: one turn per pattern, within it one per prefix length (LIVE at most).
    while k < patterns.len() {
        let mut e = 1;
        while e <= patterns[k].len as usize {
            if !earlier_has_prefix(patterns, k, e) {
                nodes += 1;
            }
            e += 1;
        }
        k += 1;
    }
    nodes
}

/// Some pattern before `patterns[k]` starts with its first `e` symbols.
const fn earlier_has_prefix(patterns: &[Pattern], k: usize, e: usize) -> bool {
    let mut j = 0;
    // Bounded: one turn per earlier pattern, within it `e` compares (LIVE at most).
    while j < k {
        if patterns[j].len as usize >= e {
            let mut at = 0;
            while at < e && patterns[j].syms[at] == patterns[k].syms[at] {
                at += 1;
            }
            if at == e {
                return true;
            }
        }
        j += 1;
    }
    false
}

/// The sequences as one deterministic automaton: `next[node][symbol]` is the node after
/// `symbol`, or [`DEAD`]; node 0 is the root. `ends[node]` is the first sequence that ends at
/// the node and `also[seq]` the next one there (equal sequences of different rules).
pub(crate) struct Trie<const NODES: usize, const SYMS: usize, const SEQS: usize> {
    next: [[Node; SYMS]; NODES],
    ends: [u16; NODES],
    also: [u16; SEQS],
}

impl<const NODES: usize, const SYMS: usize, const SEQS: usize> Trie<NODES, SYMS, SEQS> {
    /// The trie of `patterns`, sequence `k` being `patterns[k]`. Panics, which in a `static` or
    /// `const` is a compile error, when the sizes do not fit the patterns.
    pub(crate) const fn build(patterns: &[Pattern]) -> Self {
        assert!(patterns.len() == SEQS, "SEQS is the number of patterns");
        assert!(NODES <= DEAD as usize, "node numbers fit a `Node` below DEAD");
        assert!(SEQS < NO_SEQ as usize, "sequence numbers fit below NO_SEQ");
        let mut next = [[DEAD; SYMS]; NODES];
        let mut ends = [NO_SEQ; NODES];
        let mut also = [NO_SEQ; SEQS];
        let mut used = 1usize;
        let mut k = 0;
        // Bounded: one turn per pattern, and within it one per symbol.
        while k < patterns.len() {
            let pattern = patterns[k];
            assert!(pattern.len > 0 && pattern.len as usize <= LIVE, "a sequence of 1 to LIVE symbols");
            let mut node = 0usize;
            let mut e = 0;
            while e < pattern.len as usize {
                let sym = pattern.syms[e] as usize;
                assert!(sym < SYMS, "a symbol past the alphabet");
                if next[node][sym] == DEAD {
                    assert!(used < NODES, "more nodes than NODES");
                    next[node][sym] = used as Node;
                    used += 1;
                }
                node = next[node][sym] as usize;
                e += 1;
            }
            also[k] = ends[node];
            ends[node] = k as u16;
            k += 1;
        }
        assert!(used == NODES, "NODES is nodes_for(patterns): every row used");
        Trie { next, ends, also }
    }

    /// The transition from the root on `sym`, or [`DEAD`]: when no state is live, a token
    /// costs this one load.
    #[inline(always)]
    pub(crate) fn root(&self, sym: u8) -> Node {
        self.next[0][sym as usize]
    }

    /// The first sequence that ends at `node`, or [`NO_SEQ`].
    #[inline(always)]
    pub(crate) fn first_end(&self, node: Node) -> u16 {
        self.ends[node as usize]
    }

    /// The sequence after `seq` that ends where it does, or [`NO_SEQ`].
    #[inline(always)]
    pub(crate) fn next_end(&self, seq: u16) -> u16 {
        self.also[seq as usize]
    }
}

/// The trie states live after the symbols read so far: each a node and the token its match
/// started at, the oldest start first.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Live {
    node: [Node; LIVE],
    start: [u32; LIVE],
    len: u8,
}

impl Live {
    pub(crate) const fn new() -> Live {
        Live { node: [0; LIVE], start: [0; LIVE], len: 0 }
    }

    #[inline(always)]
    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline(always)]
    pub(crate) fn len(&self) -> usize {
        self.len as usize
    }

    #[inline(always)]
    pub(crate) fn clear(&mut self) {
        self.len = 0;
    }

    /// Starts one live state from the root's transition `to` at `token` (when `self.is_empty()`).
    #[inline(always)]
    pub(crate) fn start_root(&mut self, to: Node, token: u32) {
        self.node[0] = to;
        self.start[0] = token;
        self.len = 1;
    }

    /// State `at`: its node and the token its match started at.
    #[inline(always)]
    pub(crate) fn state(&self, at: usize) -> (Node, u32) {
        (self.node[at], self.start[at])
    }

    /// Every live state and a new one at the root, advanced by `sym`, the symbol of token
    /// `token`; those with no transition dropped. Afterwards each state is a node this symbol
    /// reached: the sequences ending at it ([`Trie::first_end`]) end at this token.
    ///
    /// At most `LIVE` states are kept: two states have different starts, so different depths,
    /// and a depth past the longest pattern (at most `LIVE`, checked by [`Trie::build`]) has no
    /// node. The writes are therefore in bounds, and `out <= at` holds throughout, so compaction
    /// runs in place with zero stack-array copies.
    #[inline(always)]
    pub(crate) fn step<const NODES: usize, const SYMS: usize, const SEQS: usize>(
        &mut self,
        trie: &Trie<NODES, SYMS, SEQS>,
        sym: u8,
        token: u32,
    ) {
        let column = sym as usize;
        let cur_len = self.len as usize;
        if cur_len == 0 {
            let to = trie.next[0][column];
            if to != DEAD {
                self.node[0] = to;
                self.start[0] = token;
                self.len = 1;
            }
            return;
        }
        let mut out = 0usize;
        // Bounded: LIVE turns at most.
        for at in 0..cur_len {
            let to = trie.next[self.node[at] as usize][column];
            if to != DEAD {
                self.node[out] = to;
                self.start[out] = self.start[at];
                out += 1;
            }
        }
        let to = trie.next[0][column];
        if to != DEAD {
            self.node[out] = to;
            self.start[out] = token;
            out += 1;
        }
        self.len = out as u8;
    }
}
