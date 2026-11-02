//! Lowering a compiled grammar's EBNF rules to the BNF productions a
//! parser works on, remembering where each production came from so that
//! a derivation can be read back in terms of the grammar's own rules,
//! groups and repetitions.
//!
//! Every [`Sequence`] of a rule becomes one production of that rule.
//! A parenthesised [`Item::Group`] becomes a fresh nonterminal with one
//! production per alternative, and an [`Item::Repeat`] a fresh
//! nonterminal producing exactly the counts it allows. A narrow
//! repetition is unrolled, one production per count; a wide one would
//! take a number of symbols quadratic in its bound that way, so it is
//! built from parts instead, as Lark's `_generate_repeats` factorises
//! its large repeats: nonterminals for exactly a power of two items
//! (each two of the one below), for zero or a power of two, and for
//! up to one less than a power of two (an optional power of two for each
//! bit), so that the minimum count is a sum of powers of two and the
//! range above it splits at its highest power of two, all in a number
//! of productions logarithmic in the bound. An unbounded repetition is
//! the minimum count followed by a left-recursive part adding one item
//! at a time. The parts carry [`Provenance::RepeatPart`], and a
//! derivation's reader splices their items into the repetition's own.
//! The lowering makes no attempt to follow Lark's own EBNF expansion; it
//! only needs to accept the same strings, and derives each count in one
//! way only.

use super::compile::{Grammar, Item, RuleId, Sequence, TermId};
use alloc::vec::Vec;

/// A nonterminal of the lowered grammar: the rules of the compiled
/// grammar, by [`RuleId`], followed by the fresh ones for groups and
/// repetitions.
pub type NtId = usize;

pub type ProdId = usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Symbol {
    Terminal(TermId),
    Nonterminal(NtId),
}

/// What a derivation by a production stands for in the grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// Alternative `alt` of the rule `id`.
    Rule { id: RuleId, alt: usize },
    /// Alternative `alt` of a parenthesised group.
    Group { alt: usize },
    /// Some number of repetitions of a repeated item: one per symbol of
    /// the right-hand side, or as many as the parts there derive.
    Repeat,
    /// Part of a repetition: some of its repetitions, to be read as
    /// items of the whole.
    RepeatPart,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Production {
    pub lhs: NtId,
    pub rhs: Vec<Symbol>,
    pub provenance: Provenance,
}

/// The productions of a lowered grammar, indexed two ways.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lowered {
    pub productions: Vec<Production>,
    /// The productions of each nonterminal, by [`NtId`].
    pub by_lhs: Vec<Vec<ProdId>>,
}

/// The most right-hand-side symbols a bounded repetition is unrolled
/// to, one production per count, before it is built from parts.
const UNROLL_LIMIT: u64 = 64;

struct Lowering {
    productions: Vec<Production>,
    nonterminals: usize,
}

/// The parts a wide repetition of one symbol is built from, each made
/// once per repetition.
struct Parts {
    symbol: Symbol,
    /// `powers[i]` derives exactly `2^i` items; `powers[0]` is the item.
    powers: Vec<Symbol>,
    /// `optional[i]` derives no items or `2^i` of them.
    optional: Vec<Symbol>,
    /// `below[k]` derives any count under `2^k`.
    below: Vec<Symbol>,
}

impl Lowering {
    fn fresh(&mut self) -> NtId {
        self.nonterminals += 1;
        self.nonterminals - 1
    }

    fn sequence(&mut self, seq: &Sequence) -> Vec<Symbol> {
        seq.items.iter().map(|item| self.item(item)).collect()
    }

    fn item(&mut self, item: &Item) -> Symbol {
        match item {
            Item::Terminal(id) => Symbol::Terminal(*id),
            Item::Rule(id) => Symbol::Nonterminal(*id),
            Item::Group(alts) => {
                let lhs = self.fresh();
                for (alt, seq) in alts.iter().enumerate() {
                    let rhs = self.sequence(seq);
                    self.productions.push(Production {
                        lhs,
                        rhs,
                        provenance: Provenance::Group { alt },
                    });
                }
                Symbol::Nonterminal(lhs)
            }
            Item::Repeat { inner, min, max } => {
                let symbol = self.item(inner);
                let lhs = self.fresh();
                match *max {
                    Some(max) if unrolled_symbols(*min, max) <= UNROLL_LIMIT => {
                        self.counts(lhs, symbol, *min, max);
                    }
                    _ => {
                        let mut parts = Parts::new(symbol);
                        let mut rhs = parts.exact(self, *min);
                        match *max {
                            None => rhs.push(parts.any(self)),
                            Some(max) if max > *min => rhs.push(parts.up_to(self, max - *min)),
                            Some(_) => {}
                        }
                        self.productions.push(Production {
                            lhs,
                            rhs,
                            provenance: Provenance::Repeat,
                        });
                    }
                }
                Symbol::Nonterminal(lhs)
            }
        }
    }

    /// One production of `lhs` for each count from `min` to `max` copies
    /// of `symbol`.
    fn counts(&mut self, lhs: NtId, symbol: Symbol, min: u32, max: u32) {
        for count in min..=max {
            self.productions.push(Production {
                lhs,
                rhs: alloc::vec![symbol; count as usize],
                provenance: Provenance::Repeat,
            });
        }
    }

    /// A production of the part `lhs`.
    fn part(&mut self, lhs: NtId, rhs: Vec<Symbol>) {
        self.productions.push(Production {
            lhs,
            rhs,
            provenance: Provenance::RepeatPart,
        });
    }
}

impl Parts {
    fn new(symbol: Symbol) -> Parts {
        Parts {
            symbol,
            powers: alloc::vec![symbol],
            optional: Vec::new(),
            below: Vec::new(),
        }
    }

    /// The part deriving exactly `2^i` items.
    fn power(&mut self, lowering: &mut Lowering, i: usize) -> Symbol {
        while self.powers.len() <= i {
            let half = *self.powers.last().unwrap();
            let lhs = lowering.fresh();
            lowering.part(lhs, alloc::vec![half, half]);
            self.powers.push(Symbol::Nonterminal(lhs));
        }
        self.powers[i]
    }

    /// The part deriving no items or `2^i` of them.
    fn optional(&mut self, lowering: &mut Lowering, i: usize) -> Symbol {
        while self.optional.len() <= i {
            let power = self.power(lowering, self.optional.len());
            let lhs = lowering.fresh();
            lowering.part(lhs, Vec::new());
            lowering.part(lhs, alloc::vec![power]);
            self.optional.push(Symbol::Nonterminal(lhs));
        }
        self.optional[i]
    }

    /// The part deriving any count of items under `2^k`.
    fn below(&mut self, lowering: &mut Lowering, k: usize) -> Symbol {
        while self.below.len() <= k {
            let k = self.below.len();
            let lhs = lowering.fresh();
            let rhs = match k.checked_sub(1) {
                Some(bit) => alloc::vec![self.optional(lowering, bit), self.below[bit]],
                None => Vec::new(),
            };
            lowering.part(lhs, rhs);
            self.below.push(Symbol::Nonterminal(lhs));
        }
        self.below[k]
    }

    /// The parts deriving exactly `n` items between them: one for each
    /// bit of `n`, highest first.
    fn exact(&mut self, lowering: &mut Lowering, n: u32) -> Vec<Symbol> {
        (0..u32::BITS as usize)
            .rev()
            .filter(|i| n & (1 << i) != 0)
            .map(|i| self.power(lowering, i))
            .collect()
    }

    /// The part deriving any count of items up to `d`, which is at
    /// least one: every count under the highest power of two in `d`, or
    /// that power of two and then any count up to the rest.
    fn up_to(&mut self, lowering: &mut Lowering, d: u32) -> Symbol {
        let k = d.ilog2() as usize;
        let rest = d - (1 << k);
        if rest == (1 << k) - 1 {
            return self.below(lowering, k + 1);
        }
        let lhs = lowering.fresh();
        let under = self.below(lowering, k);
        lowering.part(lhs, alloc::vec![under]);
        let mut rhs = alloc::vec![self.power(lowering, k)];
        if rest > 0 {
            rhs.push(self.up_to(lowering, rest));
        }
        lowering.part(lhs, rhs);
        Symbol::Nonterminal(lhs)
    }

    /// The part deriving any number of items at all.
    fn any(&mut self, lowering: &mut Lowering) -> Symbol {
        let lhs = lowering.fresh();
        lowering.part(lhs, Vec::new());
        lowering.part(lhs, alloc::vec![Symbol::Nonterminal(lhs), self.symbol]);
        Symbol::Nonterminal(lhs)
    }
}

/// The right-hand-side symbols of a repetition of `min` to `max` items
/// unrolled to one production per count.
fn unrolled_symbols(min: u32, max: u32) -> u64 {
    (u64::from(min) + u64::from(max)) * (u64::from(max - min) + 1) / 2
}

/// Lower `grammar` to BNF.
pub fn lower(grammar: &Grammar) -> Lowered {
    let mut lowering = Lowering {
        productions: Vec::new(),
        nonterminals: grammar.rules.len(),
    };
    for (id, rule) in grammar.rules.iter().enumerate() {
        for (alt, seq) in rule.alternatives.iter().enumerate() {
            let rhs = lowering.sequence(seq);
            lowering.productions.push(Production {
                lhs: id,
                rhs,
                provenance: Provenance::Rule { id, alt },
            });
        }
    }
    let productions = lowering.productions;
    let mut by_lhs = alloc::vec![Vec::new(); lowering.nonterminals];
    for (id, production) in productions.iter().enumerate() {
        by_lhs[production.lhs].push(id);
    }
    Lowered {
        productions,
        by_lhs,
    }
}

#[cfg(test)]
#[path = "../../../tests/embedded/native/lark/lower_tests.rs"]
mod tests;
