//! Static analysis of a compiled grammar for the generator: the smallest
//! string and the shallowest nesting each symbol can produce, which
//! symbols can produce arbitrarily long strings, and the removal of
//! alternatives that can never produce a finite string at all.
//!
//! Sizes are counted in characters, with every terminal counted at the
//! shortest string it can match: the generator steers the structure of a
//! string with these numbers and leaves the length of each terminal to the
//! terminal's own draw.

use super::compile::{Grammar, Item, RuleId, Sequence, TermId, collect_rule_refs};
use super::{GrammarError, GrammarResult};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::Cell;

/// A size bound that may be unbounded.
pub type Bound = Option<u64>;

/// A rule's right-hand side as the generator walks it: alternatives in
/// increasing order of their smallest size, each annotated with the
/// numbers the generator needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub name: String,
    pub min_size: u64,
    /// `min_size` with every terminal counted at the length of its strings
    /// on average rather than its shortest: what the generator budgets by.
    pub typical_size: u64,
    pub min_depth: u32,
    pub alternatives: Vec<Alt>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alt {
    pub min_size: u64,
    /// `min_size` with every terminal counted at the length of its strings
    /// on average rather than its shortest: what the generator budgets by.
    pub typical_size: u64,
    /// The largest string the alternative can produce with every terminal
    /// at its shortest; `None` when it can grow without bound.
    pub max_size: Bound,
    pub min_depth: u32,
    pub items: Vec<Node>,
}

/// The index of a group (a parenthesised choice of alternatives within
/// an alternative) among all the groups of a grammar.
pub type GroupId = usize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
    Terminal(TermId),
    Rule(RuleId),
    Group(GroupId, Vec<Alt>),
    Repeat {
        inner: Box<Node>,
        min: u32,
        max: Option<u32>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub kind: NodeKind,
    pub min_size: u64,
    /// `min_size` with every terminal counted at the length of its strings
    /// on average rather than its shortest: what the generator budgets by.
    pub typical_size: u64,
    pub max_size: Bound,
    pub min_depth: u32,
}

impl Alt {
    /// Whether the alternative can produce arbitrarily long strings.
    pub fn growing(&self) -> bool {
        self.max_size.is_none()
    }
}

/// The analysed grammar: rules indexed by [`RuleId`] as in the compiled
/// grammar they came from, and how many groups they hold between them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Analysis {
    pub rules: Vec<Rule>,
    pub groups: usize,
}

fn add_bound(a: Bound, b: Bound) -> Bound {
    Some(a?.saturating_add(b?))
}

fn max_bound(a: Bound, b: Bound) -> Bound {
    Some(a?.max(b?))
}

/// The per-rule quantities the fixpoints iterate over. `None` for a size
/// means "not yet known to be finite"; a rule still at `None` once the
/// iteration settles can never produce a finite string.
#[derive(Clone)]
struct RuleFacts {
    min_size: Option<u64>,
    typical_size: Option<u64>,
    min_depth: Option<u32>,
    max_size: Bound,
    unbounded: bool,
}

/// Which length of each terminal the size of a construct counts it at.
#[derive(Clone, Copy)]
enum Measure {
    /// The shortest string the terminal can produce.
    Min,
    /// The length of the terminal's strings on average.
    Typical,
}

struct Analyzer<'g> {
    grammar: &'g Grammar,
    /// The smallest string each terminal can match.
    term_min: Vec<u64>,
    /// The length of each terminal's strings on average.
    term_typical: Vec<u64>,
    facts: Vec<RuleFacts>,
    /// The rules in the order the fixpoints visit them: the rules a rule
    /// uses before it, as far as the grammar's cycles allow.
    order: Vec<RuleId>,
    /// The groups numbered so far.
    groups: Cell<usize>,
}

/// The rules in postorder of a walk from each along the rules it uses,
/// so that a rule comes after every rule it uses except through a
/// cycle. One round of a fixpoint in this order settles every rule not
/// on a cycle, however the grammar orders its definitions.
fn dependency_order(grammar: &Grammar) -> Vec<RuleId> {
    let uses: Vec<Vec<RuleId>> = grammar
        .rules
        .iter()
        .map(|rule| {
            let mut refs = Vec::new();
            for seq in &rule.alternatives {
                for item in &seq.items {
                    collect_rule_refs(item, &mut refs);
                }
            }
            refs
        })
        .collect();
    let mut order = Vec::with_capacity(grammar.rules.len());
    let mut visited = alloc::vec![false; grammar.rules.len()];
    for root in 0..grammar.rules.len() {
        if visited[root] {
            continue;
        }
        visited[root] = true;
        let mut stack = alloc::vec![(root, 0)];
        while let Some((rule, next)) = stack.last_mut() {
            if let Some(&used) = uses[*rule].get(*next) {
                *next += 1;
                if !visited[used] {
                    visited[used] = true;
                    stack.push((used, 0));
                }
            } else {
                order.push(*rule);
                stack.pop();
            }
        }
    }
    order
}

impl Analyzer<'_> {
    /// The smallest string an item can produce, counting terminals by
    /// `measure`, under the current estimates for the rules.
    fn item_size(&self, item: &Item, measure: Measure) -> Option<u64> {
        match item {
            Item::Terminal(t) => Some(match measure {
                Measure::Min => self.term_min[*t],
                Measure::Typical => self.term_typical[*t],
            }),
            Item::Rule(r) => match measure {
                Measure::Min => self.facts[*r].min_size,
                Measure::Typical => self.facts[*r].typical_size,
            },
            Item::Group(alts) => alts.iter().filter_map(|a| self.seq_size(a, measure)).min(),
            Item::Repeat { inner, min, .. } => {
                if *min == 0 {
                    Some(0)
                } else {
                    Some(
                        self.item_size(inner, measure)?
                            .saturating_mul(u64::from(*min)),
                    )
                }
            }
        }
    }

    fn seq_size(&self, seq: &Sequence, measure: Measure) -> Option<u64> {
        let mut total = 0u64;
        for item in &seq.items {
            total = total.saturating_add(self.item_size(item, measure)?);
        }
        Some(total)
    }

    fn item_min_size(&self, item: &Item) -> Option<u64> {
        self.item_size(item, Measure::Min)
    }

    fn seq_min_size(&self, seq: &Sequence) -> Option<u64> {
        self.seq_size(seq, Measure::Min)
    }

    fn item_min_depth(&self, item: &Item) -> Option<u32> {
        match item {
            Item::Terminal(_) => Some(0),
            Item::Rule(r) => self.facts[*r].min_depth,
            Item::Group(alts) => alts.iter().filter_map(|a| self.seq_min_depth(a)).min(),
            Item::Repeat { inner, min, .. } => {
                if *min == 0 {
                    Some(0)
                } else {
                    self.item_min_depth(inner)
                }
            }
        }
    }

    fn seq_min_depth(&self, seq: &Sequence) -> Option<u32> {
        let mut depth = 0u32;
        for item in &seq.items {
            depth = depth.max(self.item_min_depth(item)?);
        }
        Some(depth)
    }

    /// The largest string an item can produce under the current estimates
    /// of the rules' maxima, or `None` when it is known to be unbounded. An
    /// unproductive rule contributes nothing, since no finite string can
    /// pass through it. A terminal counts for at least one character: only
    /// a placeholder's is counted as shorter, and its value is the
    /// caller's, so a repetition of one still produces something.
    fn item_max_size(&self, item: &Item) -> Bound {
        match item {
            Item::Terminal(t) => Some(self.term_min[*t].max(1)),
            Item::Rule(r) => {
                let facts = &self.facts[*r];
                if facts.min_size.is_none() {
                    Some(0)
                } else if facts.unbounded {
                    None
                } else {
                    facts.max_size
                }
            }
            Item::Group(alts) => {
                let mut best = Some(0);
                for alt in alts {
                    if self.seq_min_size(alt).is_some() {
                        best = max_bound(best, self.seq_max_size(alt));
                    }
                }
                best
            }
            Item::Repeat { inner, max, .. } => {
                let inner_max = self.item_max_size(inner)?;
                match max {
                    Some(max) => Some(inner_max.saturating_mul(u64::from(*max))),
                    None if inner_max == 0 => Some(0),
                    None => None,
                }
            }
        }
    }

    fn seq_max_size(&self, seq: &Sequence) -> Bound {
        let mut total = Some(0);
        for item in &seq.items {
            total = add_bound(total, self.item_max_size(item));
        }
        total
    }

    fn rule_alternatives(&self, r: RuleId) -> impl Iterator<Item = &Sequence> {
        self.grammar.rules[r]
            .alternatives
            .iter()
            .filter(|alt| self.seq_min_size(alt).is_some())
    }

    /// Iterate the smallest-size and smallest-depth estimates to a
    /// fixpoint. Both start unknown and only ever decrease, so a rule whose
    /// estimate is still unknown afterwards has no finite derivation.
    fn solve_minima(&mut self) {
        loop {
            let mut changed = false;
            for i in 0..self.order.len() {
                let r = self.order[i];
                let alternatives = &self.grammar.rules[r].alternatives;
                let min_size = alternatives
                    .iter()
                    .filter_map(|alt| self.seq_size(alt, Measure::Min))
                    .min();
                let typical_size = alternatives
                    .iter()
                    .filter_map(|alt| self.seq_size(alt, Measure::Typical))
                    .min();
                let min_depth = self
                    .rule_alternatives(r)
                    .filter_map(|alt| self.seq_min_depth(alt))
                    .min()
                    .map(|d| d.saturating_add(1));
                let facts = &mut self.facts[r];
                if min_size != facts.min_size
                    || typical_size != facts.typical_size
                    || min_depth != facts.min_depth
                {
                    facts.min_size = min_size;
                    facts.typical_size = typical_size;
                    facts.min_depth = min_depth;
                    changed = true;
                }
            }
            if !changed {
                return;
            }
        }
    }

    /// Iterate the largest-size estimates upward. A finite system settles
    /// within one round per rule; any rule still rising after that many
    /// rounds lies on a cycle that adds characters, so it (and everything
    /// that reaches it) is unbounded.
    fn solve_maxima(&mut self) {
        let rounds = self.grammar.rules.len() + 1;
        for _ in 0..rounds {
            if !self.max_round() {
                return;
            }
        }
        loop {
            let mut changed = false;
            for i in 0..self.order.len() {
                let r = self.order[i];
                if self.facts[r].unbounded || self.facts[r].min_size.is_none() {
                    continue;
                }
                let next = self.rule_max_size(r);
                if next != self.facts[r].max_size {
                    self.facts[r].unbounded = true;
                    changed = true;
                }
            }
            if !changed {
                return;
            }
        }
    }

    fn rule_max_size(&self, r: RuleId) -> Bound {
        let mut best = Some(0);
        for alt in self.rule_alternatives(r) {
            best = max_bound(best, self.seq_max_size(alt));
        }
        best
    }

    fn max_round(&mut self) -> bool {
        let mut changed = false;
        for i in 0..self.order.len() {
            let r = self.order[i];
            if self.facts[r].min_size.is_none() {
                continue;
            }
            let next = self.rule_max_size(r);
            if next != self.facts[r].max_size {
                self.facts[r].max_size = next;
                changed = true;
            }
        }
        changed
    }

    fn build_item(&self, item: &Item) -> Node {
        let kind = match item {
            Item::Terminal(t) => NodeKind::Terminal(*t),
            Item::Rule(r) => NodeKind::Rule(*r),
            Item::Group(alts) => {
                let group = self.groups.replace(self.groups.get() + 1);
                NodeKind::Group(group, self.build_alts(alts))
            }
            Item::Repeat { inner, min, max } => {
                let productive = self.item_min_size(inner).is_some();
                NodeKind::Repeat {
                    inner: Box::new(self.build_item(inner)),
                    min: if productive { *min } else { 0 },
                    max: if productive { *max } else { Some(0) },
                }
            }
        };
        Node {
            kind,
            min_size: self.item_min_size(item).unwrap_or(0),
            typical_size: self.item_size(item, Measure::Typical).unwrap_or(0),
            max_size: self.item_max_size(item),
            min_depth: self.item_min_depth(item).unwrap_or(0),
        }
    }

    fn build_alts(&self, alts: &[Sequence]) -> Vec<Alt> {
        let mut built: Vec<Alt> = alts
            .iter()
            .filter(|alt| self.seq_min_size(alt).is_some())
            .map(|alt| Alt {
                min_size: self.seq_min_size(alt).unwrap_or(0),
                typical_size: self.seq_size(alt, Measure::Typical).unwrap_or(0),
                max_size: self.seq_max_size(alt),
                min_depth: self.seq_min_depth(alt).unwrap_or(0),
                items: alt.items.iter().map(|item| self.build_item(item)).collect(),
            })
            .collect();
        built.sort_by_key(|alt| (alt.min_size, alt.growing()));
        built
    }
}

/// Analyse `grammar`, with `term_min` giving the shortest string each
/// terminal can produce and `term_typical` the length of its strings on
/// average. Fails when a start rule has no finite derivation.
pub fn analyze(
    grammar: &Grammar,
    term_min: Vec<u64>,
    term_typical: Vec<u64>,
) -> GrammarResult<Analysis> {
    let mut analyzer = Analyzer {
        grammar,
        term_min,
        term_typical,
        facts: alloc::vec![
            RuleFacts {
                min_size: None,
                typical_size: None,
                min_depth: None,
                max_size: Some(0),
                unbounded: false,
            };
            grammar.rules.len()
        ],
        order: dependency_order(grammar),
        groups: Cell::new(0),
    };
    analyzer.solve_minima();
    for &start in &grammar.start {
        if analyzer.facts[start].min_size.is_none() {
            return Err(GrammarError::new(format!(
                "Rule '{}' can never produce a finite string: every alternative \
                 recurses without a base case",
                grammar.rules[start].name
            )));
        }
    }
    analyzer.solve_maxima();
    let rules = grammar
        .rules
        .iter()
        .enumerate()
        .map(|(r, rule)| Rule {
            name: rule.name.clone(),
            min_size: analyzer.facts[r].min_size.unwrap_or(0),
            typical_size: analyzer.facts[r].typical_size.unwrap_or(0),
            min_depth: analyzer.facts[r].min_depth.unwrap_or(0),
            alternatives: analyzer.build_alts(&rule.alternatives),
        })
        .collect();
    Ok(Analysis {
        rules,
        groups: analyzer.groups.get(),
    })
}

#[cfg(test)]
#[path = "../../../tests/embedded/native/lark/analysis_tests.rs"]
mod tests;
