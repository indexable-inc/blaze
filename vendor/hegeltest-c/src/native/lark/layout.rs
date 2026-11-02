//! Where a derivation's parts lie in the text it derives: the view of a
//! [`Tree`] that grammar-directed shrinking edits through. Every subtree
//! gets the range of text its leaves cover, and the layout answers the
//! structural questions the shrinker asks — which rule nodes contain a
//! shorter node of the same rule (a hoist), what the repetitions of each
//! repeat are, where the ignored text lies and which leaves make up the
//! text — so that the shrinker's candidates are splices of the text and
//! the parser decides which of them are still in the grammar.

use super::compile::{RuleId, TermId};
use super::parse::Tree;
use crate::native::HashSet;
use alloc::vec::Vec;
use core::hash::{Hash, Hasher};
use rustc_hash::FxHasher;

/// What a node of the layout is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Rule(RuleId),
    Group,
    Repeat,
    Terminal(TermId),
    Ignored(TermId),
}

/// A subtree: what it is, the text it derives — from its first to its
/// last terminal, so ignored text at its edges belongs to no node but
/// its own leaf — and its children (as indices into the layout).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub part: Part,
    pub start: usize,
    pub end: usize,
    pub children: Vec<usize>,
}

/// The nodes of a derivation in pre-order, each with its range of text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    nodes: Vec<Node>,
}

impl Layout {
    /// Lay `tree` out over its text: a node per subtree in pre-order, each
    /// with the range of text its leaves cover, which is empty for a node
    /// deriving no text of its own. The leaves of a tree cover its text
    /// in order, so a leaf starts where the one before it ends. The walk
    /// keeps its own stack of open subtrees, so a derivation nesting as
    /// deeply as its text is long costs no call stack.
    pub fn new(tree: &Tree) -> Layout {
        let mut layout = Layout { nodes: Vec::new() };
        let mut derives: Vec<bool> = Vec::new();
        let mut open: Vec<(&Tree, usize, usize)> = Vec::new();
        let mut cursor = 0;
        layout.open(tree, cursor, &mut derives);
        open.push((tree, 0, 0));
        while let Some(&(subtree, index, next)) = open.last() {
            let children = subtree.children();
            if next < children.len() {
                let top = open.len() - 1;
                open[top].2 += 1;
                let child = &children[next];
                let child_index = layout.open(child, cursor, &mut derives);
                layout.nodes[index].children.push(child_index);
                open.push((child, child_index, 0));
                continue;
            }
            open.pop();
            if let Tree::Terminal { end, .. } | Tree::Ignored { end, .. } = subtree {
                layout.nodes[index].end = *end;
                derives[index] = matches!(subtree, Tree::Terminal { .. });
                cursor = *end;
            }
            if let Some(&(_, parent, _)) = open.last() {
                if derives[index] {
                    if !derives[parent] {
                        layout.nodes[parent].start = layout.nodes[index].start;
                        derives[parent] = true;
                    }
                    layout.nodes[parent].end = layout.nodes[index].end;
                }
            }
        }
        layout
    }

    /// Add the node for `tree`, starting at text position `at` with no
    /// children yet, and return its index.
    fn open(&mut self, tree: &Tree, at: usize, derives: &mut Vec<bool>) -> usize {
        let part = match tree {
            Tree::Terminal { id, .. } => Part::Terminal(*id),
            Tree::Ignored { id, .. } => Part::Ignored(*id),
            Tree::Rule { id, .. } => Part::Rule(*id),
            Tree::Group { .. } => Part::Group,
            Tree::Repeat { .. } => Part::Repeat,
        };
        self.nodes.push(Node {
            part,
            start: at,
            end: at,
            children: Vec::new(),
        });
        derives.push(false);
        self.nodes.len() - 1
    }

    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    /// The pairs of a rule node and a node of the same rule inside it
    /// covering strictly less text, as `(ancestor, descendant)` indices:
    /// outermost ancestor first, and for each ancestor the descendant
    /// covering the least text first. A node is paired with the 1st,
    /// 2nd, 4th, 8th, … nodes of its rule above it rather than every
    /// one: `d` nodes of one rule nested in a chain would otherwise make
    /// `d²/2` pairs, each a whole text for a shrinker to try, where these
    /// make `d log d` and still reach any node from the outermost in a
    /// few accepted hoists.
    pub fn hoists(&self) -> Vec<(usize, usize)> {
        let mut pairs = Vec::new();
        let mut ancestors: Vec<(usize, RuleId)> = Vec::new();
        let mut open: Vec<(usize, usize)> = Vec::new();
        self.enter_hoists(0, &mut ancestors, &mut pairs);
        open.push((0, 0));
        while let Some(&(index, next)) = open.last() {
            let node = &self.nodes[index];
            if next < node.children.len() {
                let top = open.len() - 1;
                open[top].1 += 1;
                let child = node.children[next];
                self.enter_hoists(child, &mut ancestors, &mut pairs);
                open.push((child, 0));
                continue;
            }
            open.pop();
            if matches!(node.part, Part::Rule(_)) {
                ancestors.pop();
            }
        }
        pairs.sort_by_key(|&(a, d)| (a, self.nodes[d].end - self.nodes[d].start));
        pairs
    }

    /// Pair the node at `index`, when it is a rule node, with the nodes
    /// of its rule among `ancestors` at the distances [`Self::hoists`]
    /// takes, and make it an ancestor of the nodes within it.
    fn enter_hoists(
        &self,
        index: usize,
        ancestors: &mut Vec<(usize, RuleId)>,
        pairs: &mut Vec<(usize, usize)>,
    ) {
        let node = &self.nodes[index];
        let Part::Rule(id) = node.part else {
            return;
        };
        let mut distance = 0usize;
        for &(ancestor, ancestor_rule) in ancestors.iter().rev() {
            if ancestor_rule != id {
                continue;
            }
            distance += 1;
            let above = &self.nodes[ancestor];
            if distance.is_power_of_two() && node.end - node.start < above.end - above.start {
                pairs.push((ancestor, index));
            }
        }
        ancestors.push((index, id));
    }

    /// For each repeat node in pre-order, the ranges of its repetitions
    /// (its children other than the ignored text between them) in order.
    pub fn repeats(&self) -> Vec<Vec<(usize, usize)>> {
        self.nodes
            .iter()
            .filter(|node| node.part == Part::Repeat)
            .map(|node| {
                node.children
                    .iter()
                    .map(|&child| &self.nodes[child])
                    .filter(|child| !matches!(child.part, Part::Ignored(_)))
                    .map(|child| (child.start, child.end))
                    .collect()
            })
            .collect()
    }

    /// The ranges of ignored text, in text order.
    pub fn ignored(&self) -> Vec<(usize, usize)> {
        self.nodes
            .iter()
            .filter(|node| matches!(node.part, Part::Ignored(_)))
            .map(|node| (node.start, node.end))
            .collect()
    }

    /// Every leaf — a terminal's text or ignored text — with the terminal
    /// it matched, in text order.
    pub fn leaves(&self) -> Vec<(TermId, usize, usize)> {
        self.nodes
            .iter()
            .filter_map(|node| match node.part {
                Part::Terminal(id) | Part::Ignored(id) => Some((id, node.start, node.end)),
                _ => None,
            })
            .collect()
    }

    /// How deeply the rules nest: the most rule nodes on any path from
    /// the root to a leaf.
    pub fn depth(&self) -> u32 {
        let mut depths = alloc::vec![0u32; self.nodes.len()];
        for (index, node) in self.nodes.iter().enumerate().rev() {
            let below = node.children.iter().map(|&c| depths[c]).max().unwrap_or(0);
            depths[index] = u32::from(matches!(node.part, Part::Rule(_))) + below;
        }
        depths[0]
    }

    /// Every rule node with its rule, in pre-order.
    pub fn rules(&self) -> Vec<(RuleId, usize, usize)> {
        self.nodes
            .iter()
            .filter_map(|node| match node.part {
                Part::Rule(id) => Some((id, node.start, node.end)),
                _ => None,
            })
            .collect()
    }

    /// The coarse moves on the text for a shrinker that tries each once
    /// rather than searching: all the ignored text cut, every repeat
    /// emptied, halved and shortened by one, every hoist, and every leaf
    /// replaced by the smallest text known for its terminal.
    pub fn coarse_moves(&self) -> Vec<Coarse> {
        let mut moves = Vec::new();
        if !self.ignored().is_empty() {
            moves.push(Coarse::Ignored);
        }
        for items in self.repeats() {
            let Some(last) = items.last() else {
                continue;
            };
            let half = items.len() / 2;
            moves.push(Coarse::Delete {
                start: items[0].0,
                end: last.1,
            });
            if half > 0 {
                moves.push(Coarse::Delete {
                    start: items[items.len() - half].0,
                    end: last.1,
                });
                moves.push(Coarse::Delete {
                    start: items[items.len() - 1].0,
                    end: last.1,
                });
            }
        }
        for (ancestor, descendant) in self.hoists() {
            let (a, d) = (&self.nodes[ancestor], &self.nodes[descendant]);
            moves.push(Coarse::Hoist {
                start: a.start,
                end: a.end,
                from: d.start,
                to: d.end,
            });
        }
        for (id, start, end) in self.leaves() {
            moves.push(Coarse::Smallest { id, start, end });
        }
        moves
    }

    /// The texts the coarse moves (see [`Self::coarse_moves`]) make of
    /// `text`, with `smallest` the smallest text known for a terminal:
    /// each distinct one once, none equal to `text`, and each made only
    /// as it is asked for, since every one is as long as the text and
    /// there are as many as the derivation has nodes.
    pub fn coarse_candidates<'a>(
        &'a self,
        text: &'a [u32],
        smallest: impl Fn(TermId) -> Option<Vec<u32>> + 'a,
    ) -> impl Iterator<Item = Vec<u32>> + 'a {
        let mut seen: HashSet<u64> = HashSet::default();
        self.coarse_moves().into_iter().filter_map(move |mv| {
            let candidate = match mv {
                Coarse::Ignored => cut(text, &self.ignored()),
                Coarse::Delete { start, end } => splice(text, start, end, &[]),
                Coarse::Hoist {
                    start,
                    end,
                    from,
                    to,
                } => splice(text, start, end, &text[from..to]),
                Coarse::Smallest { id, start, end } => splice(text, start, end, &smallest(id)?),
            };
            let mut hasher = FxHasher::default();
            candidate.hash(&mut hasher);
            (candidate != text && seen.insert(hasher.finish())).then_some(candidate)
        })
    }
}

/// A coarse move on a text (see [`Layout::coarse_moves`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coarse {
    /// All the ignored text cut.
    Ignored,
    /// The text from `start` to `end`, some of a repeat's repetitions,
    /// deleted.
    Delete { start: usize, end: usize },
    /// The text from `start` to `end` replaced by the text from `from` to
    /// `to` within it, a node of the same rule.
    Hoist {
        start: usize,
        end: usize,
        from: usize,
        to: usize,
    },
    /// The leaf from `start` to `end` replaced by the smallest text known
    /// for its terminal.
    Smallest {
        id: TermId,
        start: usize,
        end: usize,
    },
}

/// `text` with `start..end` replaced by `with`.
pub fn splice(text: &[u32], start: usize, end: usize, with: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(text.len() - (end - start) + with.len());
    out.extend_from_slice(&text[..start]);
    out.extend_from_slice(with);
    out.extend_from_slice(&text[end..]);
    out
}

/// `text` with each of `ranges` (disjoint, in text order) removed.
pub fn cut(text: &[u32], ranges: &[(usize, usize)]) -> Vec<u32> {
    let mut out = Vec::with_capacity(text.len());
    let mut at = 0;
    for &(start, end) in ranges {
        out.extend_from_slice(&text[at..start]);
        at = end;
    }
    out.extend_from_slice(&text[at..]);
    out
}

#[cfg(test)]
#[path = "../../../tests/embedded/native/lark/layout_tests.rs"]
mod tests;
