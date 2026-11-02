//! Parsing a string against a compiled grammar: an Earley parser with a
//! dynamic lexer, accepting the strings `Lark(grammar, parser="earley")`
//! does with its default `dynamic` lexer, and recovering a derivation
//! tree for the shrinker to work on.
//!
//! The parser is a port of Lark's `xearley.py`: one Earley set per
//! character position, items keyed by production, dot and origin, with
//! Scott's held completions so that nullable rules complete into items
//! predicted after them. There is no tokeniser. At each position every
//! item waiting on a terminal matches that terminal's own regex there
//! (see [`Lexer::match_terminal`]), the advanced item is held back until
//! the column where the match ends, and every `%ignore` terminal is
//! matched there too, carrying the waiting items (and a completed start
//! rule) past the ignored text. A string is accepted when a completed
//! start production spanning all of it is in the last column, including
//! when it ends in ignored text. What matches a terminal is the parser's
//! caller's to say ([`Terminals`]): a [`Lexer`] matches each terminal's
//! own pattern as Lark does, and never a terminal without a pattern (one
//! `%declare`d) or one whose regex is too involved to run within the
//! matcher's budget.
//!
//! Right recursion is taken with Leo's shortcut, which plain Earley
//! lacks (and Lark's too): completing `x` at a column whose only item
//! waiting on it ends with that `x` can only complete that item, and so
//! on up, so the parser adds the topmost item of such a chain at once
//! (its [`Back::Leo`]) instead of one item per link in every column,
//! which would be a square of the text's length for `start: "a" start |
//! "a"`. The links are put back when the derivation is built.
//!
//! The tree recovered is the first derivation found, with the grammar's
//! own structure: a node per rule alternative, per parenthesised group
//! and per repetition, and leaves for the terminals and for the ignored
//! stretches between them. Ambiguity and priorities play no part in
//! acceptance, so neither resolves which derivation is returned.

use super::compile::{Grammar, RuleId, TermId};
use super::lexsim::{Lexed, Lexer};
use super::lower::{Lowered, NtId, Provenance, Symbol, lower};
use crate::native::{HashMap, HashSet};
use alloc::vec::Vec;

/// What matches a grammar's terminals in a text, for the parser to scan
/// with.
pub trait Terminals {
    /// What the matching keeps about one text from position to position:
    /// what [`begin`](Self::begin) returns for a text is handed to
    /// [`ends`](Self::ends) at every position of that text.
    type Memo;

    /// Begin matching in `text`.
    fn begin(&self, text: &[char]) -> Self::Memo;

    /// The positions where a token of `id` starting at `pos` may end, in
    /// increasing order, appended to `out`, each past `pos`: none when
    /// the terminal does not match there.
    fn ends(
        &self,
        memo: &mut Self::Memo,
        id: TermId,
        text: &[char],
        pos: usize,
        out: &mut Vec<usize>,
    );
}

/// Each terminal's own pattern matched at the position, as CPython's
/// `pattern.match` and so Lark's dynamic lexer match it: one end, and
/// never an empty one.
impl Terminals for Lexer {
    type Memo = ();

    fn begin(&self, _text: &[char]) {}

    fn ends(&self, _memo: &mut (), id: TermId, text: &[char], pos: usize, out: &mut Vec<usize>) {
        if let Lexed::Token(_, end) = self.match_terminal(id, text, pos) {
            if end > pos {
                out.push(end);
            }
        }
    }
}

/// A derivation of a string. The children of a [`Tree::Rule`] are the
/// items of the alternative's [`Sequence`](super::compile::Sequence) in
/// order, those of a [`Tree::Group`] the items of the chosen alternative,
/// and those of a [`Tree::Repeat`] one tree per repetition, each
/// interleaved with the [`Tree::Ignored`] leaves for ignored text lying
/// between them. The leaves of a tree, in order, cover the parsed text
/// exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tree {
    Rule {
        id: RuleId,
        alt: usize,
        children: Vec<Tree>,
    },
    Group {
        alt: usize,
        children: Vec<Tree>,
    },
    Repeat {
        children: Vec<Tree>,
    },
    /// A terminal's text, as character positions.
    Terminal {
        id: TermId,
        start: usize,
        end: usize,
    },
    /// A stretch of text matched by a `%ignore` terminal.
    Ignored {
        id: TermId,
        start: usize,
        end: usize,
    },
}

impl Tree {
    pub fn children(&self) -> &[Tree] {
        match self {
            Tree::Rule { children, .. }
            | Tree::Group { children, .. }
            | Tree::Repeat { children } => children,
            Tree::Terminal { .. } | Tree::Ignored { .. } => &[],
        }
    }

    fn take_children(&mut self) -> Vec<Tree> {
        match self {
            Tree::Rule { children, .. }
            | Tree::Group { children, .. }
            | Tree::Repeat { children } => core::mem::take(children),
            Tree::Terminal { .. } | Tree::Ignored { .. } => Vec::new(),
        }
    }

    /// How many nodes the tree has, leaves included.
    pub fn size(&self) -> usize {
        let mut count = 0;
        let mut pending = alloc::vec![self];
        while let Some(tree) = pending.pop() {
            count += 1;
            pending.extend(tree.children());
        }
        count
    }
}

impl Drop for Tree {
    /// Frees the subtrees from a stack of its own, so a derivation
    /// nesting as deeply as its text is long is freed without a call
    /// stack frame per level.
    fn drop(&mut self) {
        let mut pending = self.take_children();
        while let Some(mut subtree) = pending.pop() {
            let mut children = subtree.take_children();
            pending.append(&mut children);
        }
    }
}

/// Where a parse failed: the position past which no item could be
/// carried, the end of the text when it ended before the start rule was
/// complete, or the position reached when the parse ran out of a budget
/// (see [`PARSE_BUDGET`] and [`ITEM_BUDGET`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseError {
    pub position: usize,
}

/// How many items a parse may propose before it is given up, counting
/// an item as often as it is proposed, since proposing an item its
/// column already holds is most of the work on an ambiguous grammar: an
/// Earley parse is cubic in the text on one (`e: e "+" e | "1"` proposes
/// about n³/6 items for n tokens, which is the budget at some 850
/// tokens and a second of parsing, and tens of billions at the default
/// string size), so a string that would take minutes to check is
/// rejected instead, as a string the grammar does not derive is. The
/// fixture grammars' strings at the default size propose under a
/// hundred thousand.
pub const PARSE_BUDGET: usize = 100_000_000;

/// How many distinct items a parse may hold before it is given up the
/// same way. The proposal budget bounds the time, since on an ambiguous
/// grammar most proposals find their item in the column already, but a
/// deep derivation proposes mostly new ones — a rule nested `d` deep
/// over `n` tokens holds about `n·d` — at some hundred bytes each; the
/// strings the default caps allow hold a few million. This also bounds
/// queued scans and the steps used to reconstruct the derivation: shared
/// nullable items can expand to a tree much larger than the chart.
pub const ITEM_BUDGET: usize = 1 << 22;

/// A parser for one start rule of a compiled grammar.
pub struct Parser {
    lowered: Lowered,
    start: NtId,
    ignore: Vec<TermId>,
    /// How many terminals the grammar has, for the memo of what each
    /// matched at a column.
    terminals: usize,
    budget: usize,
    item_budget: usize,
}

#[derive(Clone, Copy)]
enum Back {
    Predicted,
    Token {
        pred: u32,
        term: u32,
    },
    Child {
        pred: u32,
        child: u32,
    },
    Ignored {
        pred: u32,
        term: u32,
    },
    /// The topmost item of the chain of items that completing `child`
    /// can only complete, one after the other (see [`Parse::leo_top`]);
    /// [`Parse::link`] turns it into the `Child` it would have been.
    Leo {
        child: u32,
    },
}

#[derive(Clone, Copy)]
struct Item {
    prod: u32,
    dot: u32,
    origin: u32,
    col: u32,
    back: Back,
}

#[derive(Default)]
struct Column {
    /// Items that are complete or wait on a nonterminal.
    active: Vec<u32>,
    /// Items waiting on a terminal, with that terminal.
    scan: Vec<(u32, TermId)>,
    /// Items waiting on a nonterminal, with that nonterminal first: in
    /// the order they were added while the column is the one being
    /// worked on, and sorted once it is left, so that completing a
    /// nonterminal finds the items waiting on it by binary search.
    waiting: Vec<(u32, u32)>,
}

struct Delayed {
    item: u32,
    term: TermId,
    advance: bool,
}

enum Step {
    Leaf(Tree),
    Child(u32),
}

struct Frame {
    item: u32,
    steps: alloc::vec::IntoIter<Step>,
    children: Vec<Tree>,
}

struct Parse<'p, T: Terminals> {
    parser: &'p Parser,
    terminals: &'p T,
    text: &'p [char],
    /// What `terminals` keeps about the text while matching in it.
    memo: T::Memo,
    items: Vec<Item>,
    columns: Vec<Column>,
    seen: HashSet<(u32, u32, u32, u32)>,
    /// Items advanced by a match or carried over ignored text, by the
    /// position where the match ends, and how many are still to arrive.
    delayed: Vec<Vec<Delayed>>,
    pending: usize,
    /// The completed empty item of each nonterminal in the column being
    /// worked on, with that column.
    held: Vec<Option<(u32, u32)>>,
    /// Per column and nonterminal asked about, the production and origin
    /// of the topmost item completing that nonterminal there can only
    /// lead to (see [`Self::leo_top`]), once it has been found.
    leo: HashMap<(u32, u32), (u32, u32)>,
    /// The ends found for each terminal tried at the column, by terminal,
    /// as a range of `ends`.
    tried: Vec<Option<(usize, usize)>>,
    ends: Vec<usize>,
    /// How many items have been proposed, against the budget.
    proposed: usize,
    /// How many items reading the derivation has visited, against the
    /// item budget.
    built: usize,
}

impl Parser {
    /// A parser for `grammar` from the rule `start`.
    pub fn new(grammar: &Grammar, start: RuleId) -> Parser {
        Parser {
            lowered: lower(grammar),
            start,
            ignore: grammar.ignore.clone(),
            terminals: grammar.terminals.len(),
            budget: PARSE_BUDGET,
            item_budget: ITEM_BUDGET,
        }
    }

    /// The parser giving up after `budget` proposed items instead of
    /// [`PARSE_BUDGET`].
    #[cfg(test)]
    pub fn with_budget(mut self, budget: usize) -> Parser {
        self.budget = budget;
        self
    }

    /// The parser giving up once it holds more than `item_budget` items,
    /// instead of [`ITEM_BUDGET`].
    #[cfg(test)]
    pub fn with_item_budget(mut self, item_budget: usize) -> Parser {
        self.item_budget = item_budget;
        self
    }

    /// Whether `text` is in the grammar, matching terminals with
    /// `terminals` (over the same grammar's terminals).
    #[cfg(test)]
    pub fn accepts<T: Terminals>(&self, terminals: &T, text: &[char]) -> bool {
        self.parse(terminals, text).is_ok()
    }

    /// A derivation of `text`, matching terminals with `terminals` (over
    /// the same grammar's terminals).
    pub fn parse<T: Terminals>(&self, terminals: &T, text: &[char]) -> Result<Tree, ParseError> {
        let n = text.len();
        let mut parse = Parse {
            parser: self,
            terminals,
            text,
            memo: terminals.begin(text),
            items: Vec::new(),
            columns: (0..=n).map(|_| Column::default()).collect(),
            seen: HashSet::default(),
            delayed: (0..=n).map(|_| Vec::new()).collect(),
            pending: 0,
            held: alloc::vec![None; self.lowered.by_lhs.len()],
            leo: HashMap::default(),
            tried: alloc::vec![None; self.terminals],
            ends: Vec::new(),
            proposed: 0,
            built: 0,
        };
        for &prod in &self.lowered.by_lhs[self.start] {
            parse.add(0, prod as u32, 0, 0, Back::Predicted)?;
        }
        for col in 0..=n {
            if col > 0 {
                parse.arrive(col)?;
            }
            parse.tried.fill(None);
            parse.ends.clear();
            parse.predict_and_complete(col)?;
            parse.scan(col)?;
            parse.columns[col].waiting.sort_unstable();
            if col < n && parse.pending == 0 {
                return Err(ParseError { position: col });
            }
        }
        let root = parse.columns[n]
            .active
            .iter()
            .copied()
            .find(|&idx| {
                let item = parse.items[idx as usize];
                let production = &self.lowered.productions[item.prod as usize];
                item.origin == 0
                    && production.lhs == self.start
                    && item.dot as usize == production.rhs.len()
            })
            .ok_or(ParseError { position: n })?;
        parse.build(root)
    }
}

impl<T: Terminals> Parse<'_, T> {
    fn next_symbol(&self, item: &Item) -> Option<Symbol> {
        self.parser.lowered.productions[item.prod as usize]
            .rhs
            .get(item.dot as usize)
            .copied()
    }

    fn add(
        &mut self,
        col: usize,
        prod: u32,
        dot: u32,
        origin: u32,
        back: Back,
    ) -> Result<(), ParseError> {
        if self.proposed >= self.parser.budget {
            return Err(ParseError { position: col });
        }
        self.proposed += 1;
        if !self.seen.insert((col as u32, prod, dot, origin)) {
            return Ok(());
        }
        if self.items.len() >= self.parser.item_budget {
            return Err(ParseError { position: col });
        }
        let item = Item {
            prod,
            dot,
            origin,
            col: col as u32,
            back,
        };
        let idx = self.items.len() as u32;
        self.items.push(item);
        match self.next_symbol(&item) {
            Some(Symbol::Terminal(term)) => self.columns[col].scan.push((idx, term)),
            Some(Symbol::Nonterminal(nt)) => {
                self.columns[col].active.push(idx);
                self.columns[col].waiting.push((nt as u32, idx));
            }
            None => self.columns[col].active.push(idx),
        }
        Ok(())
    }

    fn arrive(&mut self, col: usize) -> Result<(), ParseError> {
        let arrivals = core::mem::take(&mut self.delayed[col]);
        self.pending -= arrivals.len();
        for arrival in arrivals {
            let pred = self.items[arrival.item as usize];
            let term = arrival.term as u32;
            if arrival.advance {
                let back = Back::Token {
                    pred: arrival.item,
                    term,
                };
                self.add(col, pred.prod, pred.dot + 1, pred.origin, back)?;
            } else {
                let back = Back::Ignored {
                    pred: arrival.item,
                    term,
                };
                self.add(col, pred.prod, pred.dot, pred.origin, back)?;
            }
        }
        Ok(())
    }

    /// Predict from and complete the column's active items, and whatever
    /// that adds.
    fn predict_and_complete(&mut self, col: usize) -> Result<(), ParseError> {
        let parser = self.parser;
        let mut processed = 0;
        while processed < self.columns[col].active.len() {
            let idx = self.columns[col].active[processed];
            processed += 1;
            let item = self.items[idx as usize];
            let next = self.next_symbol(&item);
            if let Some(Symbol::Nonterminal(nt)) = next {
                for &prod in &parser.lowered.by_lhs[nt] {
                    self.add(col, prod as u32, 0, col as u32, Back::Predicted)?;
                }
                if let Some((held_col, child)) = self.held[nt] {
                    if held_col as usize == col {
                        let back = Back::Child { pred: idx, child };
                        self.add(col, item.prod, item.dot + 1, item.origin, back)?;
                    }
                }
            } else if next.is_none() {
                let lhs = parser.lowered.productions[item.prod as usize].lhs;
                if item.origin as usize == col
                    && self.held[lhs].is_none_or(|(held_col, _)| held_col as usize != col)
                {
                    self.held[lhs] = Some((col as u32, idx));
                }
                let origin = item.origin as usize;
                if origin != col {
                    if let Some((prod, top_origin)) = self.leo_top(origin, lhs) {
                        let dot = parser.lowered.productions[prod as usize].rhs.len() as u32;
                        self.add(col, prod, dot, top_origin, Back::Leo { child: idx })?;
                        continue;
                    }
                }
                let waiting = &self.columns[origin].waiting;
                let (from, to) = if origin == col {
                    (0, waiting.len())
                } else {
                    (
                        waiting.partition_point(|&(nt, _)| (nt as usize) < lhs),
                        waiting.partition_point(|&(nt, _)| nt as usize <= lhs),
                    )
                };
                for j in from..to {
                    let (nt, pred) = self.columns[origin].waiting[j];
                    if nt as usize != lhs {
                        continue;
                    }
                    let originator = self.items[pred as usize];
                    let back = Back::Child { pred, child: idx };
                    self.add(
                        col,
                        originator.prod,
                        originator.dot + 1,
                        originator.origin,
                        back,
                    )?;
                }
            }
        }
        Ok(())
    }

    /// The one item of a column already left that waits on `nt` and ends
    /// with it, when there is exactly one such item and nothing else
    /// waits on `nt` there.
    fn sole_waiter(&self, col: usize, nt: usize) -> Option<u32> {
        let waiting = &self.columns[col].waiting;
        let from = waiting.partition_point(|&(x, _)| (x as usize) < nt);
        let to = waiting.partition_point(|&(x, _)| x as usize <= nt);
        if to != from + 1 {
            return None;
        }
        let idx = waiting[from].1;
        let item = &self.items[idx as usize];
        let rhs = &self.parser.lowered.productions[item.prod as usize].rhs;
        (item.dot as usize + 1 == rhs.len()).then_some(idx)
    }

    /// The production and origin of the topmost item that completing
    /// `nt` from `col` can only complete: when the sole item waiting on
    /// `nt` there ends with it, completing `nt` completes that item and
    /// nothing else, and if its own nonterminal is in the same position
    /// at its origin the chain goes on. `None` when there is no such
    /// chain. The chain stops short of completing the start rule from
    /// the first column, since that item is the one a parse of the whole
    /// text is found by, and the one carried past ignored text at its
    /// end; it cannot come round otherwise, because the first item of a
    /// cycle to be predicted in a column was predicted by an item outside
    /// it that waits on the same nonterminal, so that nonterminal has two
    /// waiters, and only the seeded start items are predicted by nothing.
    fn leo_top(&mut self, col: usize, nt: usize) -> Option<(u32, u32)> {
        let mut chain: Vec<((u32, u32), (u32, u32))> = Vec::new();
        let (mut col, mut nt) = (col, nt);
        let top = loop {
            if col == 0 && nt == self.parser.start {
                break None;
            }
            let key = (col as u32, nt as u32);
            if let Some(&top) = self.leo.get(&key) {
                break Some(top);
            }
            let Some(waiter) = self.sole_waiter(col, nt) else {
                break None;
            };
            let item = &self.items[waiter as usize];
            chain.push((key, (item.prod, item.origin)));
            col = item.origin as usize;
            nt = self.parser.lowered.productions[item.prod as usize].lhs;
        };
        let mut top = top;
        for &(key, link) in chain.iter().rev() {
            self.leo.insert(key, *top.get_or_insert(link));
        }
        top
    }

    /// Where `term` matched at `col` can end, as a range of `ends`,
    /// matched once per column.
    fn lex(&mut self, col: usize, term: TermId) -> (usize, usize) {
        if let Some(range) = self.tried[term] {
            return range;
        }
        let from = self.ends.len();
        self.terminals
            .ends(&mut self.memo, term, self.text, col, &mut self.ends);
        let to = self.ends.len();
        self.tried[term] = Some((from, to));
        (from, to)
    }

    fn delay(
        &mut self,
        end: usize,
        item: u32,
        term: TermId,
        advance: bool,
    ) -> Result<(), ParseError> {
        if self.items.len() + self.pending >= self.parser.item_budget {
            return Err(ParseError {
                position: self.items[item as usize].col as usize,
            });
        }
        self.delayed[end].push(Delayed {
            item,
            term,
            advance,
        });
        self.pending += 1;
        Ok(())
    }

    /// Scan the column's items waiting on a terminal, and carry those and
    /// any complete start item past the ignored text matched at the
    /// column.
    fn scan(&mut self, col: usize) -> Result<(), ParseError> {
        let parser = self.parser;
        let waiting = self.columns[col].scan.len();
        for j in 0..waiting {
            let (idx, term) = self.columns[col].scan[j];
            let (from, to) = self.lex(col, term);
            for k in from..to {
                self.delay(self.ends[k], idx, term, true)?;
            }
        }
        let active = self.columns[col].active.len();
        for &ignore in &parser.ignore {
            let (from, to) = self.lex(col, ignore);
            for k in from..to {
                let end = self.ends[k];
                for j in 0..waiting {
                    let (idx, _) = self.columns[col].scan[j];
                    self.delay(end, idx, ignore, false)?;
                }
                for j in 0..active {
                    let idx = self.columns[col].active[j];
                    let item = self.items[idx as usize];
                    let production = &parser.lowered.productions[item.prod as usize];
                    if production.lhs == parser.start && item.dot as usize == production.rhs.len() {
                        self.delay(end, idx, ignore, false)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn provenance(&self, idx: u32) -> Provenance {
        self.parser.lowered.productions[self.items[idx as usize].prod as usize].provenance
    }

    /// The steps of the derivation of a complete item, in order: a leaf
    /// for each terminal and ignored stretch, and the completed item for
    /// each nonterminal, with the steps of each part of a repetition
    /// spliced in place of the part.
    fn steps(&mut self, idx: u32) -> Result<Vec<Step>, ParseError> {
        let mut out = Vec::new();
        let mut pending = Vec::new();
        self.own_steps(idx, &mut pending)?;
        while let Some(step) = pending.pop() {
            match step {
                Step::Child(child) if self.provenance(child) == Provenance::RepeatPart => {
                    self.own_steps(child, &mut pending)?;
                }
                step => out.push(step),
            }
        }
        Ok(out)
    }

    /// Put back the items a `Leo` chain skipped, from `child` up to the
    /// item at `idx`, as the `Child` items plain completion would have
    /// added, so that the derivation can be read off their steps.
    fn link(&mut self, idx: u32) -> Result<(), ParseError> {
        let Back::Leo { child } = self.items[idx as usize].back else {
            return Ok(());
        };
        let top = self.items[idx as usize];
        let mut child = child;
        loop {
            let completed = self.items[child as usize];
            let lhs = self.parser.lowered.productions[completed.prod as usize].lhs;
            let Some(pred) = self.sole_waiter(completed.origin as usize, lhs) else {
                unreachable!("every link of a Leo chain has a sole waiter")
            };
            let waiter = self.items[pred as usize];
            let back = Back::Child { pred, child };
            if (waiter.prod, waiter.origin) == (top.prod, top.origin) {
                self.items[idx as usize].back = back;
                return Ok(());
            }
            if self.items.len() >= self.parser.item_budget {
                return Err(ParseError {
                    position: top.col as usize,
                });
            }
            self.items.push(Item {
                prod: waiter.prod,
                dot: waiter.dot + 1,
                origin: waiter.origin,
                col: top.col,
                back,
            });
            child = self.items.len() as u32 - 1;
        }
    }

    /// The steps of a complete item's own production, pushed last first
    /// so that the first is on top.
    fn own_steps(&mut self, idx: u32, pending: &mut Vec<Step>) -> Result<(), ParseError> {
        let mut at = idx;
        loop {
            if self.built >= self.parser.item_budget {
                return Err(ParseError {
                    position: self.items[idx as usize].col as usize,
                });
            }
            self.built += 1;
            self.link(at)?;
            let item = &self.items[at as usize];
            let end = item.col as usize;
            match item.back {
                Back::Predicted => return Ok(()),
                Back::Token { pred, term } => {
                    let start = self.items[pred as usize].col as usize;
                    let id = term as TermId;
                    pending.push(Step::Leaf(Tree::Terminal { id, start, end }));
                    at = pred;
                }
                Back::Ignored { pred, term } => {
                    let start = self.items[pred as usize].col as usize;
                    let id = term as TermId;
                    pending.push(Step::Leaf(Tree::Ignored { id, start, end }));
                    at = pred;
                }
                Back::Child { pred, child } => {
                    pending.push(Step::Child(child));
                    at = pred;
                }
                Back::Leo { .. } => unreachable!("a Leo item is linked before it is read"),
            }
        }
    }

    fn frame(&mut self, item: u32) -> Result<Frame, ParseError> {
        Ok(Frame {
            item,
            steps: self.steps(item)?.into_iter(),
            children: Vec::new(),
        })
    }

    fn node(&self, frame: Frame) -> Tree {
        let children = frame.children;
        match self.provenance(frame.item) {
            Provenance::Rule { id, alt } => Tree::Rule { id, alt, children },
            Provenance::Group { alt } => Tree::Group { alt, children },
            Provenance::Repeat | Provenance::RepeatPart => Tree::Repeat { children },
        }
    }

    fn build(&mut self, root: u32) -> Result<Tree, ParseError> {
        let mut root_frame = self.frame(root)?;
        let mut stack: Vec<Frame> = Vec::new();
        loop {
            let frame = stack.last_mut().unwrap_or(&mut root_frame);
            match frame.steps.next() {
                Some(Step::Leaf(tree)) => frame.children.push(tree),
                Some(Step::Child(child)) => {
                    let next = self.frame(child)?;
                    stack.push(next);
                }
                None => match stack.pop() {
                    Some(done) => {
                        let tree = self.node(done);
                        stack
                            .last_mut()
                            .unwrap_or(&mut root_frame)
                            .children
                            .push(tree);
                    }
                    None => return Ok(self.node(root_frame)),
                },
            }
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/embedded/native/lark/parse_tests.rs"]
mod tests;
