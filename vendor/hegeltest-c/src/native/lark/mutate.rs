//! Growing a string under a grammar out of its own derivation, the
//! grammar's counterpart of the engine's span mutation: where span
//! mutation duplicates and swaps the choices under same-label spans, a
//! grammar string has no spans to speak of, so the same moves are made
//! on the text through its [`Layout`] — a rule node's text copied over
//! another node of the same rule (a nested one to deepen a recursion),
//! or a repetition duplicated. Every proposal is a splice of the text
//! that the parser then judges, so a move the dynamic lexer would read
//! differently proposes nothing.

use super::compile::RuleId;
use super::layout::{Layout, Node, Part, splice};
use crate::native::HashMap;
use crate::native::rng::EngineRng;
use alloc::vec::Vec;
use rand::RngExt;

/// The structural moves open on a derivation, each a way to grow (or
/// reshape) the text while plausibly staying in the grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Move {
    /// Copy the text of one rule node over another node of the same
    /// rule, as `(source, target)` indices into the layout.
    CopyRule(usize, usize),
    /// Insert a copy of the repetition at `(start, end)` of the text at
    /// `at`, so that a repeat has one more item.
    Duplicate { start: usize, end: usize, at: usize },
}

/// The moves open on a layout, indexable without being listed: a copy
/// between each ordered pair of distinct nodes of the same rule, then a
/// duplication of each repetition (with the text between it and its
/// neighbour, so that separated repetitions stay separated).
pub struct Moves {
    /// The nodes of each rule that has at least two, in pre-order.
    same_rule: Vec<Vec<usize>>,
    duplicates: Vec<Move>,
}

impl Moves {
    pub fn of(layout: &Layout) -> Moves {
        let mut by_rule: HashMap<RuleId, Vec<usize>> = HashMap::default();
        for (index, node) in layout.nodes().iter().enumerate() {
            if let Part::Rule(id) = node.part {
                by_rule.entry(id).or_default().push(index);
            }
        }
        let mut same_rule: Vec<Vec<usize>> =
            by_rule.into_values().filter(|v| v.len() >= 2).collect();
        same_rule.sort();
        let mut duplicates = Vec::new();
        for items in layout.repeats() {
            for (k, &(start, end)) in items.iter().enumerate() {
                let (start, end) = match (k.checked_sub(1).map(|p| items[p]), items.get(k + 1)) {
                    (Some((_, previous_end)), _) => (previous_end, end),
                    (None, Some(&(next_start, _))) => (start, next_start),
                    (None, None) => (start, end),
                };
                duplicates.push(Move::Duplicate {
                    start,
                    end,
                    at: end,
                });
            }
        }
        Moves {
            same_rule,
            duplicates,
        }
    }

    pub fn len(&self) -> usize {
        self.same_rule
            .iter()
            .map(|nodes| nodes.len() * (nodes.len() - 1))
            .sum::<usize>()
            + self.duplicates.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Move `k` of [`Self::len`]: the copies rule by rule, source-major,
    /// then the duplications.
    pub fn get(&self, mut k: usize) -> Move {
        for nodes in &self.same_rule {
            let count = nodes.len() * (nodes.len() - 1);
            if k < count {
                let (source, mut target) = (k / (nodes.len() - 1), k % (nodes.len() - 1));
                if target >= source {
                    target += 1;
                }
                return Move::CopyRule(nodes[source], nodes[target]);
            }
            k -= count;
        }
        self.duplicates[k]
    }
}

/// `text` after `mv`, a move on its layout.
pub fn apply(layout: &Layout, text: &[u32], mv: Move) -> Vec<u32> {
    match mv {
        Move::CopyRule(source, target) => {
            let (Node { start, end, .. }, target) =
                (&layout.nodes()[source], &layout.nodes()[target]);
            splice(text, target.start, target.end, &text[*start..*end])
        }
        Move::Duplicate { start, end, at } => splice(text, at, at, &text[start..end]),
    }
}

/// A random move on `text` under `layout` that changes it, or `None`
/// when the layout offers none.
pub fn grow(layout: &Layout, text: &[u32], rng: &mut EngineRng) -> Option<Vec<u32>> {
    let moves = Moves::of(layout);
    if moves.is_empty() {
        return None;
    }
    let mv = moves.get(rng.random_range(0..moves.len()));
    Some(apply(layout, text, mv)).filter(|grown| grown != text)
}

#[cfg(test)]
#[path = "../../../tests/embedded/native/lark/mutate_tests.rs"]
mod tests;
