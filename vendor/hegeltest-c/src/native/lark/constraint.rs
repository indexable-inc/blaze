//! The grammar constraint a string choice can carry: a string drawn from
//! a Lark grammar is one string node in the choice sequence, and its
//! kind, a [`StringChoice`](crate::native::core::StringChoice) with a
//! [`GrammarConstraint`], accepts exactly the strings the grammar derives.
//! Validating a replayed string means parsing it, so the constraint owns
//! the compiled grammar, the [`Lexer`] the parser matches its terminals
//! through and a [`Parser`] from its start rule, and
//! remembers the derivations it has found so that a string known to
//! derive is never parsed twice. From every derivation it also learns
//! the smallest text it has seen each rule and each terminal derive,
//! which grammar-directed shrinking proposes in place of a larger one.
//!
//! Two constraints compare equal when they were compiled from the same
//! grammar, for the same start rule, size cap and alphabet: the engine compares
//! kinds across executions (see
//! [`KindLedger`](crate::native::exec_cache::KindLedger)), and a test
//! that compiles its grammar afresh each run must not look nondeterministic.

use super::ast::PatternKind;
use super::compile::{Grammar, RuleId, TermId, placeholder};
use super::layout::{Layout, Node, Part};
use super::lexsim::Lexer;
use super::parse::{Parser, Tree};
use crate::control::InternalError;
use crate::native::HashMap;
use crate::native::core::Drawn;
use crate::native::intervalsets::IntervalSet;
use crate::sys::sync::Mutex;
use alloc::boxed::Box;
use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;
use core::hash::{Hash, Hasher};
use once_cell::race::OnceBox;
use rustc_hash::FxHasher;

/// How many derivations a constraint remembers before forgetting them all
/// to make room, and how many nodes they may have between them: a
/// derivation of a string at the default size cap has thousands, each a
/// few words.
pub const KNOWN_CAPACITY: usize = 512;
pub const KNOWN_NODES: usize = 1 << 20;

/// The derivations a constraint remembers, with their nodes counted.
struct Known {
    trees: HashMap<Vec<u32>, Arc<Tree>>,
    nodes: usize,
    node_cap: usize,
}

/// A grammar constraint on a string choice; see the module docs.
pub struct GrammarConstraint {
    grammar: Grammar,
    lexer: Lexer,
    parser: Parser,
    /// The placeholders of the explicit terminals the grammar does not
    /// ignore, which a string keeps apart from the tokens beside them
    /// (see [`GrammarConstraint::tree`]).
    placeholders: Vec<TermId>,
    start: RuleId,
    identity: u64,
    max_size: usize,
    /// How deep the rules of a string's derivation may nest, for
    /// mutation (see [`crate::native::lark::mutate`]); the generator keeps
    /// to it, and the parser does not, since an ambiguous grammar may
    /// derive a generated string more deeply than the walk did. It is no
    /// part of the identity: which strings the grammar accepts does not
    /// depend on it.
    max_depth: u32,
    /// The simplest string, computed by the generator at the first draw
    /// (see [`GrammarConstraint::simplest_or_init`]).
    simplest: OnceBox<Vec<u32>>,
    known: Mutex<Known>,
    intervals: Arc<IntervalSet>,
    smallest: Mutex<Smallest>,
    /// The characters terminals other than placeholders may contain, when
    /// the generator was given an alphabet.
    alphabet: Option<IntervalSet>,
    drawn: Drawn,
}

/// The smallest text seen derived by each rule and each terminal, in
/// the shrink order of the strings the constraint is on: shorter first,
/// then character by character in the alphabet's shrink order.
#[derive(Default)]
struct Smallest {
    rules: Vec<Option<Vec<u32>>>,
    terminals: Vec<Option<Vec<u32>>>,
}

impl GrammarConstraint {
    /// The constraint accepting the strings `grammar` derives from the
    /// rule `start` within `max_size` characters whose terminals, other
    /// than placeholders, are over `alphabet`. Its simplest string is
    /// the generator's to supply (see
    /// [`simplest_or_init`](Self::simplest_or_init)) before any choice
    /// is drawn under it.
    pub fn new(
        grammar: Grammar,
        start: RuleId,
        max_size: usize,
        max_depth: u32,
        alphabet: Option<&IntervalSet>,
    ) -> Result<GrammarConstraint, InternalError> {
        let lexer = Lexer::new(&grammar.terminals);
        let parser = Parser::new(&grammar, start);
        let placeholders = grammar
            .terminals
            .iter()
            .enumerate()
            .filter(|(id, term)| term.is_placeholder() && !grammar.ignore.contains(id))
            .map(|(id, _)| id)
            .collect();
        let intervals = match alphabet {
            None => IntervalSet::unicode_scalars()?,
            Some(alphabet) => {
                let mut marked: Vec<u32> = grammar
                    .terminals
                    .iter()
                    .filter(|term| term.is_placeholder())
                    .flat_map(|term| {
                        placeholder(&term.name)
                            .chars()
                            .map(u32::from)
                            .collect::<Vec<_>>()
                    })
                    .collect();
                marked.sort_unstable();
                marked.dedup();
                let marks = IntervalSet::new(marked.into_iter().map(|cp| (cp, cp)).collect())?;
                alphabet.union(&marks)?
            }
        };
        let mut hasher = FxHasher::default();
        format!("{grammar:?}").hash(&mut hasher);
        start.hash(&mut hasher);
        max_size.hash(&mut hasher);
        alphabet.map(|a| &a.intervals).hash(&mut hasher);
        let smallest = Smallest {
            rules: alloc::vec![None; grammar.rules.len()],
            terminals: alloc::vec![None; grammar.terminals.len()],
        };
        Ok(GrammarConstraint {
            grammar,
            lexer,
            parser,
            placeholders,
            start,
            identity: hasher.finish(),
            max_size,
            max_depth,
            simplest: OnceBox::new(),
            known: Mutex::new(Known {
                trees: HashMap::default(),
                nodes: 0,
                node_cap: KNOWN_NODES,
            }),
            intervals: Arc::new(intervals),
            smallest: Mutex::new(smallest),
            alphabet: alphabet.cloned(),
            drawn: Drawn::default(),
        })
    }

    /// The alphabet of the strings the constraint is on: every Unicode
    /// scalar value, the grammar alone deciding which strings are in, or,
    /// under an alphabet, that alphabet and the characters of the
    /// placeholders' markers.
    pub fn intervals(&self) -> &Arc<IntervalSet> {
        &self.intervals
    }

    /// The simplest string the grammar derives, computing it with `init`
    /// when it is not set yet: the one the generator draws when every
    /// decision goes the simplest way, which stands in for a replayed
    /// string the grammar rejects. Once set it is never computed again.
    pub fn simplest_or_init<E>(
        &self,
        init: impl FnOnce() -> Result<Vec<u32>, E>,
    ) -> Result<&[u32], E> {
        let simplest = self
            .simplest
            .get_or_try_init(|| init().map(Box::new))
            .map(|simplest| simplest.as_slice())?;
        self.tree(simplest);
        Ok(simplest)
    }

    /// Whether `text` is in the grammar: drawn by the generator, known to
    /// derive, or parsed now and remembered. A codepoint that is not a
    /// Unicode scalar value is in no grammar.
    pub fn accepts(&self, text: &[u32]) -> bool {
        self.drawn.contains(text) || self.tree(text).is_some()
    }

    /// Note `text` as a string the generator drew (see [`Drawn`]).
    pub fn note_drawn(&self, text: &[u32]) {
        self.drawn.note(text);
    }

    /// A derivation of `text`, remembered or found now, or `None` when
    /// the grammar does not derive it, when it exceeds the size cap,
    /// when a terminal other than a placeholder has a character outside
    /// the alphabet, or when the grammar ignores some terminal and a
    /// placeholder in `text` has a token right beside it:
    /// the generator keeps ignored text between them, since what the
    /// caller puts in for the placeholder is beyond its lexing checks,
    /// so such a string is not one the grammar draws.
    pub fn tree(&self, text: &[u32]) -> Option<Arc<Tree>> {
        if let Some(tree) = self.known.lock().trees.get(text) {
            return Some(Arc::clone(tree));
        }
        let chars: Vec<char> = text
            .iter()
            .map(|&cp| char::from_u32(cp))
            .collect::<Option<_>>()?;
        let tree = Arc::new(self.parser.parse(&self.lexer, &chars).ok()?);
        let layout = Layout::new(&tree);
        if !self.within_cap(&layout, text)
            || !self.placeholders_apart(&layout)
            || !self.over_alphabet(&layout, text)
        {
            return None;
        }
        self.learn(&layout, text);
        let mut known = self.known.lock();
        let size = tree.size();
        if known.trees.len() >= KNOWN_CAPACITY || known.nodes + size > known.node_cap {
            known.trees.clear();
            known.nodes = 0;
        }
        known.nodes += size;
        known.trees.insert(text.to_vec(), Arc::clone(&tree));
        Some(tree)
    }

    /// The leaves of `layout`, in text order.
    fn leaves(layout: &Layout) -> Vec<&Node> {
        layout
            .nodes()
            .iter()
            .filter(|node| matches!(node.part, Part::Terminal(_) | Part::Ignored(_)))
            .collect()
    }

    fn is_placeholder_leaf(&self, node: &Node) -> bool {
        matches!(node.part, Part::Terminal(id) if self.placeholders.contains(&id))
    }

    /// Whether every placeholder leaf of `layout` has ignored text, or
    /// the end of the string, on either side; trivially so when the
    /// grammar ignores nothing.
    fn placeholders_apart(&self, layout: &Layout) -> bool {
        if self.placeholders.is_empty() || self.grammar.ignore.is_empty() {
            return true;
        }
        let leaves = Self::leaves(layout);
        leaves.iter().enumerate().all(|(i, leaf)| {
            !self.is_placeholder_leaf(leaf)
                || [i.checked_sub(1), Some(i + 1)]
                    .into_iter()
                    .flatten()
                    .all(|j| {
                        leaves
                            .get(j)
                            .is_none_or(|beside| matches!(beside.part, Part::Ignored(_)))
                    })
        })
    }

    /// Whether every terminal of `layout`, a derivation of `text`, other
    /// than a placeholder, is over the alphabet; trivially so without one.
    fn over_alphabet(&self, layout: &Layout, text: &[u32]) -> bool {
        let Some(alphabet) = &self.alphabet else {
            return true;
        };
        layout.leaves().into_iter().all(|(id, start, end)| {
            self.grammar.terminals[id].is_placeholder()
                || text[start..end].iter().all(|&cp| alphabet.contains(cp))
        })
    }

    /// The ignored stretches of `layout` that keep a placeholder apart
    /// from the token beside it, which no string of the grammar is
    /// without (see [`GrammarConstraint::tree`]).
    pub fn separators(&self, layout: &Layout) -> Vec<(usize, usize)> {
        let leaves = Self::leaves(layout);
        leaves
            .iter()
            .enumerate()
            .filter(|(i, leaf)| {
                matches!(leaf.part, Part::Ignored(_))
                    && [i.checked_sub(1), Some(i + 1)]
                        .into_iter()
                        .flatten()
                        .any(|j| {
                            leaves
                                .get(j)
                                .is_some_and(|beside| self.is_placeholder_leaf(beside))
                        })
            })
            .map(|(_, leaf)| (leaf.start, leaf.end))
            .collect()
    }

    /// Note the text each rule and terminal derives in `layout`, a
    /// derivation of `text`, where it is smaller than any seen before.
    fn learn(&self, layout: &Layout, text: &[u32]) {
        let mut smallest = self.smallest.lock();
        for node in layout.nodes() {
            let slot = match node.part {
                Part::Rule(id) => &mut smallest.rules[id],
                Part::Terminal(id) | Part::Ignored(id) => &mut smallest.terminals[id],
                Part::Group | Part::Repeat => continue,
            };
            let derived = &text[node.start..node.end];
            if slot
                .as_deref()
                .is_none_or(|known| self.smaller(derived, known))
            {
                *slot = Some(derived.to_vec());
            }
        }
    }

    /// Whether `a` comes before `b` in the shrink order of the strings
    /// the constraint is on.
    pub fn smaller(&self, a: &[u32], b: &[u32]) -> bool {
        let key = |cp: &u32| self.intervals.index_from_codepoint_in_shrink_order(*cp);
        a.len() < b.len() || (a.len() == b.len() && a.iter().map(key).lt(b.iter().map(key)))
    }

    /// The smallest text seen derived by `rule`.
    pub fn smallest_for_rule(&self, rule: RuleId) -> Option<Vec<u32>> {
        self.smallest.lock().rules[rule].clone()
    }

    /// The smallest text seen matched by `terminal`.
    pub fn smallest_for_terminal(&self, terminal: TermId) -> Option<Vec<u32>> {
        self.smallest.lock().terminals[terminal].clone()
    }

    /// Whether `terminal` matches one text only: a string literal with
    /// no flags, which no edit of its text keeps in the grammar.
    pub fn fixed_text(&self, terminal: TermId) -> bool {
        self.grammar.terminals[terminal]
            .pattern
            .as_ref()
            .is_some_and(|pattern| pattern.kind == PatternKind::Str && pattern.flags.is_empty())
    }

    /// The simplest string the grammar derives, once set.
    pub fn simplest(&self) -> Option<&[u32]> {
        self.simplest.get().map(|simplest| simplest.as_slice())
    }

    /// The hash of the compiled grammar, start rule, size cap and
    /// alphabet that the constraint compares and hashes by.
    pub fn identity(&self) -> u64 {
        self.identity
    }

    /// The cap on the length of the strings accepted.
    pub fn max_size(&self) -> usize {
        self.max_size
    }

    /// Whether `text`, laid out by `layout`, is within the size cap, its
    /// placeholder markers counting for nothing (see
    /// [`crate::native::lark::generate`]).
    pub fn within_cap(&self, layout: &Layout, text: &[u32]) -> bool {
        let markers: usize = layout
            .leaves()
            .into_iter()
            .filter(|(id, _, _)| self.grammar.terminals[*id].is_placeholder())
            .map(|(_, start, end)| end - start)
            .sum();
        text.len() - markers <= self.max_size
    }

    /// How deep the rules of a derivation may nest, for mutation.
    pub fn max_depth(&self) -> u32 {
        self.max_depth
    }

    /// The compiled grammar.
    pub fn grammar(&self) -> &Grammar {
        &self.grammar
    }

    /// The start rule.
    pub fn start(&self) -> RuleId {
        self.start
    }

    /// How many derivations are remembered.
    #[cfg(test)]
    pub fn known_len(&self) -> usize {
        self.known.lock().trees.len()
    }

    /// Cap the nodes of the remembered derivations at `cap` instead of
    /// [`KNOWN_NODES`].
    #[cfg(test)]
    pub fn cap_known_nodes(&self, cap: usize) {
        self.known.lock().node_cap = cap;
    }
}

impl PartialEq for GrammarConstraint {
    fn eq(&self, other: &Self) -> bool {
        self.identity == other.identity
    }
}

impl Eq for GrammarConstraint {}

impl Hash for GrammarConstraint {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.identity.hash(state);
    }
}

impl fmt::Debug for GrammarConstraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GrammarConstraint")
            .field("identity", &format_args!("{:016x}", self.identity))
            .field("start", &self.grammar.rules[self.start].name)
            .finish()
    }
}

#[cfg(test)]
#[path = "../../../tests/embedded/native/lark/constraint_tests.rs"]
mod tests;
