//! Shrinking a string under a grammar through its derivation. The string
//! passes leave such a string alone — the strings they would propose are
//! hardly ever in the grammar — and this pass edits it where its
//! derivation says the grammar might still accept the result: a rule's
//! text replaced by that of a smaller node of the same rule inside it,
//! repetitions deleted, ignored text cut, a rule's or a terminal's text
//! replaced by the smallest the constraint has seen it derive, and each
//! leaf's text shortened and lowered character by character. Every
//! candidate is a splice of the text that the parser then judges, so an
//! edit that leaves the grammar (a lexer reading two neighbouring tokens
//! as one, say) is simply rejected.

use super::search::BinSearchDown;
use super::{PassExit, ShrinkResult, Shrinker, absorb_node_gone};
use crate::control::hegel_internal_unwrap;
use crate::native::HashMap;
use crate::native::core::{ChoiceValue, StringChoice};
use crate::native::lark::constraint::GrammarConstraint;
use crate::native::lark::layout::{Layout, cut, splice};
use alloc::sync::Arc;
use alloc::vec::Vec;

/// A string under a grammar as the pass sees it between probes: its
/// kind, its constraint, its text and the derivation laid over the text.
struct Derived {
    kind: StringChoice,
    constraint: Arc<GrammarConstraint>,
    text: Vec<u32>,
    layout: Layout,
}

impl<'a> Shrinker<'a> {
    pub(super) async fn shrink_grammar_strings(&mut self) -> ShrinkResult<()> {
        let mut i = 0;
        while i < self.current_nodes.len() {
            absorb_node_gone(self.shrink_grammar_node(i).await)?;
            i += 1;
        }
        Ok(())
    }

    async fn shrink_grammar_node(&mut self, i: usize) -> Result<(), PassExit> {
        let (kind, _, current) = self.grammar_string_at(i).ok_or(PassExit::NodeGone)?;
        let simplest = kind.simplest()?;
        if simplest != current {
            self.replace_text(i, simplest).await?;
        }
        loop {
            let mut progress = self.hoist_rules(i).await?;
            progress |= self.delete_repetitions(i).await?;
            progress |= self.delete_ignored(i).await?;
            progress |= self.replace_rules_with_smallest(i).await?;
            progress |= self.shrink_leaves(i).await?;
            if !progress {
                return Ok(());
            }
        }
    }

    /// The kind, constraint and value at node `i`, or `None` when the
    /// node is not (or no longer) a string under a grammar.
    fn grammar_string_at(
        &self,
        i: usize,
    ) -> Option<(StringChoice, Arc<GrammarConstraint>, Vec<u32>)> {
        let (sc, v) = self.current_nodes.get(i)?.data.as_string()?;
        let constraint = Arc::clone(sc.constraint.as_ref()?.grammar()?);
        Some((sc.clone(), constraint, v.to_vec()))
    }

    /// Node `i` with its derivation; `NodeGone` when it is no string under
    /// a grammar, or holds a string the grammar does not derive.
    fn derivation(&self, i: usize) -> Result<Derived, PassExit> {
        let (kind, constraint, text) = self.grammar_string_at(i).ok_or(PassExit::NodeGone)?;
        let tree = constraint.tree(&text).ok_or(PassExit::NodeGone)?;
        let layout = Layout::new(&tree);
        Ok(Derived {
            kind,
            constraint,
            text,
            layout,
        })
    }

    /// Try `text` at node `i`; a text equal to the current one is no
    /// shrink and is not run.
    async fn replace_text(&mut self, i: usize, text: Vec<u32>) -> Result<bool, PassExit> {
        let (_, _, current) = self.grammar_string_at(i).ok_or(PassExit::NodeGone)?;
        if current == text {
            return Ok(false);
        }
        Ok(self
            .replace(&HashMap::from_iter([(i, ChoiceValue::String(text))]))
            .await?)
    }

    /// Replace a rule node's text with that of a node of the same rule
    /// inside it, outermost first, starting over after each accepted
    /// hoist.
    async fn hoist_rules(&mut self, i: usize) -> Result<bool, PassExit> {
        let mut changed = false;
        'again: loop {
            let d = self.derivation(i)?;
            for (ancestor, descendant) in d.layout.hoists() {
                let (a, b) = (&d.layout.nodes()[ancestor], &d.layout.nodes()[descendant]);
                let candidate = splice(&d.text, a.start, a.end, &d.text[b.start..b.end]);
                if self.replace_text(i, candidate).await? {
                    changed = true;
                    continue 'again;
                }
            }
            return Ok(changed);
        }
    }

    /// For each repeat, delete all its repetitions, then windows of 8, 4,
    /// 2 and 1 of them from the back. A repeat left with no repetitions
    /// may be gone from the new derivation, so the one now in its place
    /// is looked at next.
    async fn delete_repetitions(&mut self, i: usize) -> Result<bool, PassExit> {
        let mut changed = false;
        let mut r = 0;
        loop {
            let d = self.derivation(i)?;
            let Some(items) = d.layout.repeats().get(r).cloned() else {
                return Ok(changed);
            };
            if let (Some(first), Some(last)) = (items.first(), items.last()) {
                if self
                    .replace_text(i, splice(&d.text, first.0, last.1, &[]))
                    .await?
                {
                    changed = true;
                    continue;
                }
            }
            for k in [8usize, 4, 2, 1] {
                let mut j = usize::MAX;
                loop {
                    let d = self.derivation(i)?;
                    let items = d.layout.repeats().get(r).cloned().unwrap_or_default();
                    j = j.min(items.len());
                    if j < k || k >= items.len() {
                        break;
                    }
                    let candidate = splice(&d.text, items[j - k].0, items[j - 1].1, &[]);
                    j -= if self.replace_text(i, candidate).await? {
                        changed = true;
                        k
                    } else {
                        1
                    };
                }
            }
            r += 1;
        }
    }

    /// The ignored stretches of `d` that may go: all but those keeping a
    /// placeholder apart from the token beside it (see
    /// [`GrammarConstraint::separators`]).
    fn cuttable(d: &Derived) -> Vec<(usize, usize)> {
        let kept = d.constraint.separators(&d.layout);
        d.layout
            .ignored()
            .into_iter()
            .filter(|stretch| !kept.contains(stretch))
            .collect()
    }

    /// Cut all the ignored text that may go at once, then each stretch of
    /// it from the back.
    async fn delete_ignored(&mut self, i: usize) -> Result<bool, PassExit> {
        let d = self.derivation(i)?;
        let ignored = Self::cuttable(&d);
        if ignored.is_empty() {
            return Ok(false);
        }
        let mut changed = self.replace_text(i, cut(&d.text, &ignored)).await?;
        let mut j = usize::MAX;
        loop {
            let d = self.derivation(i)?;
            let ignored = Self::cuttable(&d);
            j = j.min(ignored.len());
            if j == 0 {
                return Ok(changed);
            }
            j -= 1;
            let (start, end) = ignored[j];
            changed |= self
                .replace_text(i, splice(&d.text, start, end, &[]))
                .await?;
        }
    }

    /// Replace each rule node's text, outermost first, with the smallest
    /// text the constraint has seen the rule derive.
    async fn replace_rules_with_smallest(&mut self, i: usize) -> Result<bool, PassExit> {
        let mut changed = false;
        let mut r = 0;
        loop {
            let d = self.derivation(i)?;
            let Some(&(rule, start, end)) = d.layout.rules().get(r) else {
                return Ok(changed);
            };
            r += 1;
            if let Some(small) = d.constraint.smallest_for_rule(rule) {
                if d.constraint.smaller(&small, &d.text[start..end]) {
                    changed |= self
                        .replace_text(i, splice(&d.text, start, end, &small))
                        .await?;
                }
            }
        }
    }

    /// Replace each leaf's text with the smallest the constraint has seen
    /// its terminal match, and shrink the text of those it cannot; a leaf
    /// of a terminal with one text only is left as it is.
    async fn shrink_leaves(&mut self, i: usize) -> Result<bool, PassExit> {
        let mut changed = false;
        let mut j = 0;
        loop {
            let d = self.derivation(i)?;
            let Some(&(terminal, start, end)) = d.layout.leaves().get(j) else {
                return Ok(changed);
            };
            j += 1;
            if d.constraint.fixed_text(terminal) {
                continue;
            }
            if let Some(small) = d.constraint.smallest_for_terminal(terminal) {
                if d.constraint.smaller(&small, &d.text[start..end])
                    && self
                        .replace_text(i, splice(&d.text, start, end, &small))
                        .await?
                {
                    changed = true;
                    continue;
                }
            }
            changed |= self.shrink_leaf(i, j - 1).await?;
        }
    }

    /// Node `i` with the range of its leaf `j`, or `None` when the
    /// derivation has fewer leaves now.
    fn leaf(&self, i: usize, j: usize) -> Result<Option<(Derived, usize, usize)>, PassExit> {
        let d = self.derivation(i)?;
        let range = d
            .layout
            .leaves()
            .get(j)
            .map(|&(_, start, end)| (start, end));
        Ok(range.map(|(start, end)| (d, start, end)))
    }

    /// Shrink the text of leaf `j` stage by stage (see [`LeafStage`])
    /// until the stages are exhausted or the leaf is gone from the
    /// derivation.
    async fn shrink_leaf(&mut self, i: usize, j: usize) -> Result<bool, PassExit> {
        let mut changed = false;
        let mut stage = LeafStage::Delete;
        while let Some((d, start, end)) = self.leaf(i, j)? {
            let Some(candidate) = stage.candidate(&d, start, end)? else {
                break;
            };
            let ok = self.replace_text(i, candidate).await?;
            stage.record(ok);
            changed |= ok;
        }
        Ok(changed)
    }
}

/// Where the shrinking of one leaf's text is: deleting it, keeping the
/// shortest prefix that will do (a binary search on the length), deleting
/// single characters from the back, then lowering each character in turn
/// as far down the alphabet's shrink order as it goes (a binary search on
/// its shrink key).
enum LeafStage {
    Delete,
    Prefix(Option<BinSearchDown>),
    Back(usize),
    Lower { p: usize, search: Option<Lowering> },
    Done,
}

/// The binary search lowering one character: the codepoint the search's
/// bounds are about, which the text should have there (the one it began
/// from, then each one accepted in its place), and the last one probed.
/// Should the text have another there, which an accepted replacement
/// re-derived differently can leave, the search starts over from it.
struct Lowering {
    expected: u32,
    probed: u32,
    search: BinSearchDown,
}

impl LeafStage {
    /// The next text to try for the leaf at `start..end` of `d`, or
    /// `None` once every stage is exhausted; [`Self::record`] must follow
    /// with the verdict.
    fn candidate(
        &mut self,
        d: &Derived,
        start: usize,
        end: usize,
    ) -> Result<Option<Vec<u32>>, PassExit> {
        let len = end - start;
        loop {
            match self {
                LeafStage::Delete => return Ok(Some(splice(&d.text, start, end, &[]))),
                LeafStage::Prefix(slot) => {
                    let search = slot.get_or_insert_with(|| BinSearchDown::new(1, len as i128));
                    match search.probe() {
                        Some(keep) if (keep as usize) < len => {
                            return Ok(Some(splice(&d.text, start + keep as usize, end, &[])));
                        }
                        Some(_) => search.record(false),
                        None => *self = LeafStage::Back(usize::MAX),
                    }
                }
                LeafStage::Back(p) => {
                    *p = (*p).min(len);
                    if *p == 0 {
                        *self = LeafStage::Lower { p: 0, search: None };
                    } else {
                        *p -= 1;
                        return Ok(Some(splice(&d.text, start + *p, start + *p + 1, &[])));
                    }
                }
                LeafStage::Lower { p, search: slot } => {
                    if *p >= len {
                        *self = LeafStage::Done;
                        continue;
                    }
                    let codepoint = d.text[start + *p];
                    if slot
                        .as_ref()
                        .is_some_and(|lowering| lowering.expected != codepoint)
                    {
                        *slot = None;
                    }
                    let key = d.kind.codepoint_key(codepoint);
                    let lowering = slot.get_or_insert_with(|| Lowering {
                        expected: codepoint,
                        probed: codepoint,
                        search: BinSearchDown::new(0, i128::from(key)),
                    });
                    match lowering.search.probe() {
                        Some(k) => {
                            let codepoint = hegel_internal_unwrap!(
                                d.kind.key_to_codepoint(k as u32),
                                "shrink_leaf: a key below a character's is in the alphabet"
                            );
                            lowering.probed = codepoint;
                            return Ok(Some(splice(
                                &d.text,
                                start + *p,
                                start + *p + 1,
                                &[codepoint],
                            )));
                        }
                        None => {
                            *p += 1;
                            *slot = None;
                        }
                    }
                }
                LeafStage::Done => return Ok(None),
            }
        }
    }

    /// Record the verdict on the text [`Self::candidate`] last returned.
    fn record(&mut self, ok: bool) {
        match self {
            LeafStage::Delete => {
                *self = if ok {
                    LeafStage::Done
                } else {
                    LeafStage::Prefix(None)
                }
            }
            LeafStage::Prefix(Some(search)) => search.record(ok),
            LeafStage::Lower {
                search: Some(lowering),
                ..
            } => {
                if ok {
                    lowering.expected = lowering.probed;
                }
                lowering.search.record(ok);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/embedded/native/shrinker_grammar_tests.rs"]
mod tests;
