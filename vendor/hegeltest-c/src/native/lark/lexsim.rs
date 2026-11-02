//! A simulation of Lark's lexers, used to check that the terminals the
//! generator concatenates would be lexed back into the same tokens: that
//! two adjacent tokens do not merge, that a keyword drawn for an
//! identifier terminal is not re-typed as the keyword, and that an
//! inserted `%ignore` string really is skipped.
//!
//! Two lexers are simulated. The dynamic lexer of Lark's default Earley
//! parser matches each terminal the parser expects, and each `%ignore`
//! terminal, with its own pattern at the current position; what it finds
//! is [`Lexer::match_terminal`], and a string every expected terminal
//! matches at its place is one Lark parses. Lark's `BasicLexer` (and the
//! contextual lexer of its LALR parser) instead sorts the terminals by
//! priority, then longest possible match, then longest pattern source,
//! then name, and takes the first that matches at the current position;
//! string terminals a regex terminal of the same priority matches
//! completely are removed from that order and recognised afterwards
//! instead (`_create_unless`): a token the regex terminal matched is
//! re-typed as the string terminal when the texts agree. That is
//! [`Lexer::next_token`], which the generator uses to prefer strings the
//! stricter lexers read back as intended.
//!
//! Both lexers are run over the same texts many times as the generator
//! tries tokens in their contexts, so a [`Scan`] records, along with what
//! a lexer found at the start of a text, how that finding depended on the
//! text's end (the terminals the end left open, and how each would go on
//! with more text), and [`Lexer::extend`] carries it over to the longer
//! texts the same token is part of without matching it again.

use super::ast::{Pattern, PatternKind};
use super::compile::{TermId, TerminalDef};
use crate::native::re::categories::class_ranges;
use crate::native::re::constants::{AtCode, MAXREPEAT, SRE_FLAG_IGNORECASE, unicode_version};
use crate::native::re::matcher::{
    MatchError, Matcher, Probe, Touch, literal_eq, literal_sequence, probe_matches, run_len,
};
use crate::native::re::parser::{OpCode, ParsedPattern, SetItem, SubPattern};
use crate::native::re::vm::{Program, Resume, has_nested_repeats};
use crate::native::re::width::get_width;
use crate::unicodedata::UnicodeVersion;
use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::cmp::Reverse;

/// The characters a pattern can start with, as sorted disjoint ranges;
/// `None` when the analysis gave up (so any character might start it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirstChars(Vec<(u32, u32)>);

impl FirstChars {
    /// How many characters the set has.
    fn len(&self) -> u64 {
        self.0.iter().map(|&(lo, hi)| u64::from(hi - lo) + 1).sum()
    }

    fn chars(&self) -> impl Iterator<Item = char> + '_ {
        self.0
            .iter()
            .flat_map(|&(lo, hi)| (lo..=hi).filter_map(char::from_u32))
    }

    fn contains(&self, c: char) -> bool {
        let c = c as u32;
        let i = self.0.partition_point(|&(_, hi)| hi < c);
        self.0.get(i).is_some_and(|&(lo, _)| lo <= c)
    }

    fn normalized(mut ranges: Vec<(u32, u32)>) -> Self {
        ranges.sort_unstable();
        let mut merged: Vec<(u32, u32)> = Vec::with_capacity(ranges.len());
        for (lo, hi) in ranges {
            match merged.last_mut() {
                Some(last) if lo <= last.1.saturating_add(1) => last.1 = last.1.max(hi),
                _ => merged.push((lo, hi)),
            }
        }
        FirstChars(merged)
    }
}

/// The result of walking a (sub)pattern for its first characters: the
/// ranges collected so far, and whether the walk is still at a point the
/// pattern could skip past without consuming anything.
struct FirstWalk {
    ranges: Vec<(u32, u32)>,
    any: bool,
    version: UnicodeVersion,
}

impl FirstWalk {
    fn seq(&mut self, p: &SubPattern) -> bool {
        for op in &p.data {
            if !self.op(op) {
                return false;
            }
        }
        true
    }

    /// Add the first characters of `op`; returns whether `op` can match
    /// the empty string, in which case the walk continues to the next op.
    fn op(&mut self, op: &OpCode) -> bool {
        match op {
            OpCode::Literal(c) => {
                self.ranges.push((*c, *c));
                false
            }
            OpCode::NotLiteral(_) | OpCode::Any | OpCode::GroupRef(_) => {
                self.any = true;
                false
            }
            OpCode::In(items) => {
                for item in items {
                    match item {
                        SetItem::Literal(c) => self.ranges.push((*c, *c)),
                        SetItem::Range(lo, hi) => self.ranges.push((*lo, *hi)),
                        SetItem::Category(category) => {
                            match class_ranges(*category, self.version) {
                                Some(ranges) => self.ranges.extend_from_slice(ranges),
                                None => self.any = true,
                            }
                        }
                        SetItem::Negate => self.any = true,
                    }
                }
                false
            }
            OpCode::At(_) | OpCode::Assert { .. } | OpCode::AssertNot { .. } => true,
            OpCode::Failure => false,
            OpCode::Branch(items) => {
                let mut nullable = false;
                for item in items {
                    nullable |= self.seq(item);
                }
                nullable
            }
            OpCode::Subpattern { p, add_flags, .. } => {
                if add_flags & SRE_FLAG_IGNORECASE != 0 {
                    self.any = true;
                }
                self.seq(p)
            }
            OpCode::AtomicGroup(p) => self.seq(p),
            OpCode::GroupRefExists { yes, no, .. } => {
                let mut nullable = self.seq(yes);
                match no {
                    Some(no) => nullable |= self.seq(no),
                    None => nullable = true,
                }
                nullable
            }
            OpCode::MaxRepeat { min, item, .. }
            | OpCode::MinRepeat { min, item, .. }
            | OpCode::PossessiveRepeat { min, item, .. } => {
                let nullable = self.seq(item);
                *min == 0 || nullable
            }
        }
    }
}

/// The characters a match of `parsed` can start with, or `None` when any
/// character might (case-insensitive patterns, categories, references).
pub fn first_chars(parsed: &ParsedPattern) -> Option<FirstChars> {
    if parsed.flags & SRE_FLAG_IGNORECASE != 0 {
        return None;
    }
    let mut walk = FirstWalk {
        ranges: Vec::new(),
        any: false,
        version: unicode_version(parsed.flags),
    };
    let nullable = walk.seq(&parsed.pattern);
    if walk.any || nullable {
        return None;
    }
    Some(FirstChars::normalized(walk.ranges))
}

/// The most first characters of a candidate [`Lexer::can_stop`] tries
/// against the run of the token before it; a larger set is taken to
/// contain one that stops the run.
const STOPPERS_TRIED: u64 = 256;

/// Whether a match of `parsed` at a position can depend on the text
/// before that position: an anchor or word boundary it may test before
/// consuming anything, or a lookbehind that may reach further back than
/// what it has consumed by then.
pub fn anchored(parsed: &ParsedPattern) -> bool {
    looks_back(&parsed.pattern, 0).is_none()
}

/// The least number of characters consumed after `p` matches at a
/// position `consumed` characters into a text, or `None` when `p` may
/// look before the start of the text.
fn looks_back(p: &SubPattern, consumed: u64) -> Option<u64> {
    let mut consumed = consumed;
    for op in &p.data {
        consumed = match op {
            OpCode::Literal(_) | OpCode::NotLiteral(_) | OpCode::Any | OpCode::In(_) => {
                consumed + 1
            }
            OpCode::At(AtCode::End | AtCode::EndString) | OpCode::GroupRef(_) | OpCode::Failure => {
                consumed
            }
            OpCode::At(_) => {
                if consumed == 0 {
                    return None;
                }
                consumed
            }
            OpCode::Assert { direction, p } | OpCode::AssertNot { direction, p } => {
                if *direction < 0 {
                    if get_width(p).1.is_none_or(|hi| hi > consumed) {
                        return None;
                    }
                    looks_back(p, 0)?;
                } else {
                    looks_back(p, consumed)?;
                }
                consumed
            }
            OpCode::Branch(alternatives) => {
                let mut least = None;
                for alternative in alternatives {
                    let after = looks_back(alternative, consumed)?;
                    least = Some(least.map_or(after, |least: u64| least.min(after)));
                }
                least.unwrap_or(consumed)
            }
            OpCode::Subpattern { p, .. } | OpCode::AtomicGroup(p) => looks_back(p, consumed)?,
            OpCode::GroupRefExists { yes, no, .. } => {
                let yes = looks_back(yes, consumed)?;
                match no {
                    Some(no) => yes.min(looks_back(no, consumed)?),
                    None => consumed,
                }
            }
            OpCode::MaxRepeat { min, item, .. }
            | OpCode::MinRepeat { min, item, .. }
            | OpCode::PossessiveRepeat { min, item, .. } => {
                let each = looks_back(item, consumed)? - consumed;
                consumed.saturating_add(each.saturating_mul(u64::from(*min)))
            }
        };
    }
    Some(consumed)
}

/// One terminal's pattern as the simulated lexers match it: the parsed
/// regex, the characters a match can start with (so most terminals are
/// ruled out at a position without running the matcher), for a
/// case-sensitive string terminal the literal itself, compared directly,
/// for any other plain sequence of literals (a string terminal with
/// flags) its code points and their flags, so that re-typing a token as
/// it compares them directly too, the compiled [`Program`] for a pattern
/// whose simulation remembers the states it passes through (one without
/// anchors or lookarounds) or that could send the backtracking matcher
/// quadratic, and whether a match can depend on the text before it. The
/// backtracking matcher is left with the rest.
#[derive(Clone)]
struct TermPattern {
    /// Shared between the clones of the pattern the lexers hold, so the
    /// addresses that identify its repeats (see [`Touch::Tail`]) agree.
    parsed: Arc<ParsedPattern>,
    first: Option<FirstChars>,
    literal: Option<Vec<char>>,
    literals: Option<(Vec<u32>, u32)>,
    program: Option<Program>,
    anchored: bool,
}

/// What matching a terminal's pattern at a position found: the end of
/// the match, how that depended on the edges of the text, and whether
/// the pattern was run at all (one its first characters rule out, or a
/// literal, looks at nothing before the position).
struct Outcome {
    result: Result<Option<usize>, MatchError>,
    edge: Edge,
    anchored: bool,
}

impl Outcome {
    fn fixed(result: Result<Option<usize>, MatchError>) -> Outcome {
        Outcome {
            result,
            edge: Edge::Fixed,
            anchored: false,
        }
    }
}

impl TermPattern {
    fn new(pattern: &Pattern, parsed: ParsedPattern) -> TermPattern {
        let literal = (pattern.kind == PatternKind::Str && pattern.flags.is_empty())
            .then(|| pattern.value.chars().collect());
        TermPattern {
            first: first_chars(&parsed),
            literals: literal
                .is_none()
                .then(|| literal_sequence(&parsed))
                .flatten(),
            program: literal
                .is_none()
                .then(|| Program::compile(&parsed).ok())
                .flatten()
                .filter(|program| program.remembers() || has_nested_repeats(&parsed.pattern)),
            anchored: anchored(&parsed),
            parsed: Arc::new(parsed),
            literal,
        }
    }

    /// Whether a match at a position could start with `c`: false only
    /// when the pattern's first characters rule it out, so that the
    /// pattern need not be run there at all. Cheap enough to ask before
    /// [`scan`](Self::scan) builds an outcome the caller would throw away.
    #[inline]
    fn may_start(&self, c: char) -> bool {
        self.first.as_ref().is_none_or(|first| first.contains(c))
    }

    /// The end of the match at `pos`, as CPython's `pattern.match` finds
    /// it.
    #[inline]
    fn match_at(&self, text: &[char], pos: usize) -> Result<Option<usize>, MatchError> {
        if text.get(pos).is_some_and(|&c| !self.may_start(c)) {
            return Ok(None);
        }
        self.scan(text, pos).result
    }

    /// [`match_at`](Self::match_at), with how the result depended on the
    /// edges of the text.
    fn scan(&self, text: &[char], pos: usize) -> Outcome {
        if let Some(first) = &self.first {
            match text.get(pos) {
                Some(&c) if first.contains(c) => {}
                Some(_) => {
                    return Outcome::fixed(Ok(None));
                }
                None => {
                    return Outcome {
                        result: Ok(None),
                        edge: Edge::Lost,
                        anchored: false,
                    };
                }
            }
        }
        if let Some(literal) = &self.literal {
            let rest = &text[pos.min(text.len())..];
            if rest.len() < literal.len() {
                let edge = if literal.starts_with(rest) {
                    Edge::Carry(Carry::Literal)
                } else {
                    Edge::Fixed
                };
                return Outcome {
                    result: Ok(None),
                    edge,
                    anchored: false,
                };
            }
            return Outcome::fixed(Ok(rest.starts_with(literal).then(|| pos + literal.len())));
        }
        let (result, edge) = if let Some(program) = &self.program {
            let (end, touch, resume) = program.scan(text, pos, None);
            let edge = match (touch, resume) {
                (Touch::Never, _) => Edge::Fixed,
                (_, Some(resume)) => Edge::Carry(Carry::Resume(resume)),
                (_, None) => Edge::Lost,
            };
            (Ok(end), edge)
        } else {
            match Matcher::new(&self.parsed).match_at_touch(text, pos) {
                Ok((end, Touch::Never)) => (Ok(end), Edge::Fixed),
                Ok((end, Touch::Tail(run_id))) => (Ok(end), Edge::Carry(Carry::Tail(run_id))),
                Ok((end, Touch::Unless(probes))) => (Ok(end), Edge::Carry(Carry::Unless(probes))),
                Ok((end, Touch::End)) => (Ok(end), Edge::Lost),
                Err(e) => (Err(e), Edge::Lost),
            }
        };
        Outcome {
            result,
            edge,
            anchored: self.anchored,
        }
    }

    /// How far the unit of the tail run `run_id` (from a [`Touch::Tail`]
    /// this pattern reported) continues from `from`.
    fn run_len(&self, run_id: usize, text: &[char], from: usize) -> Option<usize> {
        run_len(&self.parsed, run_id, text, from)
    }

    /// Whether `c` passes one of `probes` (from a [`Touch::Unless`] this
    /// pattern reported); `None` for a probe the pattern cannot run.
    fn probed(&self, probes: &[Probe], c: char) -> Option<bool> {
        for probe in probes {
            if probe_matches(&self.parsed, probe, c)? {
                return Some(true);
            }
        }
        Some(false)
    }

    /// Whether the pattern matches the whole of `text`. The lexers ask
    /// this of every string terminal a token could be re-typed as, so
    /// the comparison with a literal stays in the caller.
    #[inline]
    fn fullmatch(&self, text: &[char]) -> Result<bool, MatchError> {
        if let Some(literal) = &self.literal {
            return Ok(text == literal.as_slice());
        }
        self.fullmatch_pattern(text)
    }

    #[inline(never)]
    fn fullmatch_pattern(&self, text: &[char]) -> Result<bool, MatchError> {
        if let Some((literals, flags)) = &self.literals {
            return Ok(text.len() == literals.len()
                && literals
                    .iter()
                    .zip(text)
                    .all(|(&cp, &c)| literal_eq(cp, c, *flags)));
        }
        Matcher::new(&self.parsed).fullmatch(text)
    }
}

struct LexTerminal {
    id: TermId,
    pattern: TermPattern,
    unless: Vec<(TermId, TermPattern)>,
}

/// What the simulated lexer found at a position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lexed {
    /// A token of the given terminal ending at the given position.
    Token(TermId, usize),
    /// No terminal matches here.
    NoMatch,
    /// The matcher ran out of budget: the text is too involved to check.
    Unknown,
}

/// How a terminal's outcome at the start of a text could change with
/// more text after it, and how to find out.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Carry {
    /// The text is a proper prefix of the terminal's literal: compare
    /// the literal again.
    Literal,
    /// The backtracking matcher's unbounded run `run_id` reached the
    /// end of the text, where the match ended: the match goes on as far
    /// as the run does.
    Tail(usize),
    /// The backtracking matcher's tests that found the end: the outcome
    /// stands unless the next character passes one of them.
    Unless(Vec<Probe>),
    /// The simulation's threads still waiting for input at the end.
    Resume(Resume),
}

/// How a terminal's outcome depended on the edges of the text.
enum Edge {
    /// Not at all.
    Fixed,
    /// More text after it could change the outcome, as described.
    Carry(Carry),
    /// In a way that cannot be carried over.
    Lost,
}

/// The steps of a [`Scan`]: the terminals tried whose outcome more text
/// could change, each with how to find out. Nearly every scan notes
/// none or one, so those are kept in the scan itself and only a second
/// step moves them to the heap.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Steps {
    None,
    One((TermId, Carry)),
    Many(Vec<(TermId, Carry)>),
}

impl Steps {
    fn as_slice(&self) -> &[(TermId, Carry)] {
        match self {
            Steps::None => &[],
            Steps::One(step) => core::slice::from_ref(step),
            Steps::Many(steps) => steps,
        }
    }

    fn push(&mut self, id: TermId, carry: Carry) {
        if let Steps::Many(steps) = self {
            steps.push((id, carry));
            return;
        }
        *self = match core::mem::replace(self, Steps::None) {
            Steps::One(first) => Steps::Many(vec![first, (id, carry)]),
            _ => Steps::One((id, carry)),
        };
    }

    fn extend_from(&mut self, other: &Steps) {
        for (id, carry) in other.as_slice() {
            self.push(*id, carry.clone());
        }
    }
}

/// What a lexer found at a position of some text, with how that depended
/// on where the text ended, so that the finding can be carried over to a
/// longer text that begins the same way (see [`Lexer::extend`]), and
/// whether it could depend on text before the position.
#[derive(Debug, Clone)]
pub struct Scan {
    pub lexed: Lexed,
    /// The terminals tried whose outcome more text could change, in the
    /// order they were tried, each with how to find out; `lexed` is what
    /// stands when none of them does.
    steps: Steps,
    /// Whether the finding can be carried over at all.
    carried: bool,
    /// Whether a terminal tried could have looked at text before the
    /// position, so that the finding stands only where nothing precedes
    /// it (see [`anchored`]).
    pub anchored: bool,
}

impl Scan {
    fn new(lexed: Lexed) -> Scan {
        Scan {
            lexed,
            steps: Steps::None,
            carried: !matches!(lexed, Lexed::Unknown),
            anchored: false,
        }
    }

    fn note(&mut self, id: TermId, outcome: &Outcome) {
        self.anchored |= outcome.anchored;
        match &outcome.edge {
            Edge::Fixed => {}
            Edge::Carry(carry) => self.steps.push(id, carry.clone()),
            Edge::Lost => self.carried = false,
        }
    }

    /// Whether carrying this scan over a text (see [`Lexer::extend`])
    /// finds a token ending where carrying `other` over the same text
    /// does, whatever the text: the same terminals were left open, each
    /// with the same way to find out, and both findings carry over or
    /// neither does. The tokens found may still differ in type, as the
    /// basic lexer's re-typing or the findings themselves say.
    pub fn follows(&self, other: &Scan) -> bool {
        self.carried == other.carried && self.steps.as_slice() == other.steps.as_slice()
    }
}

/// What the lexer found for terminal `id` when matching its pattern at
/// a position gave `result`.
fn lexed(id: TermId, result: &Result<Option<usize>, MatchError>) -> Lexed {
    match result {
        Ok(Some(end)) => Lexed::Token(id, *end),
        Ok(None) => Lexed::NoMatch,
        Err(_) => Lexed::Unknown,
    }
}

/// The sorted terminal order of Lark's basic lexer and its keyword
/// re-typing, over the terminals of one grammar.
pub struct Lexer {
    terminals: Vec<LexTerminal>,
    patterns: Vec<Option<TermPattern>>,
    /// Where each terminal (by [`TermId`]) is in `terminals`.
    positions: Vec<Option<usize>>,
}

fn is_subset(flags: &str, of: &str) -> bool {
    flags.chars().all(|f| of.contains(f))
}

impl Lexer {
    /// Build the lexer over `terminals` (indexed by [`TermId`]); terminals
    /// without a pattern take no part.
    pub fn new(terminals: &[TerminalDef]) -> Lexer {
        let mut order: Vec<(TermId, &TerminalDef, TermPattern)> = Vec::new();
        let mut patterns = Vec::with_capacity(terminals.len());
        for (id, term) in terminals.iter().enumerate() {
            let pattern = term
                .parsed
                .as_ref()
                .map(|parsed| TermPattern::new(term.pattern.as_ref().unwrap(), parsed.clone()));
            if let Some(pattern) = &pattern {
                order.push((id, term, pattern.clone()));
            }
            patterns.push(pattern);
        }
        order.sort_by(|(_, a, _), (_, b, _)| {
            let key = |t: &TerminalDef| {
                let pattern = t.pattern.as_ref().unwrap();
                Reverse((
                    t.priority,
                    t.max_width.unwrap_or(u64::from(MAXREPEAT)),
                    pattern.value.chars().count(),
                ))
            };
            key(a).cmp(&key(b)).then_with(|| a.name.cmp(&b.name))
        });
        let mut embedded: Vec<TermId> = Vec::new();
        let mut unless_lists: Vec<Vec<(TermId, TermPattern)>> = Vec::new();
        for (_, term, tpattern) in &order {
            let mut unless = Vec::new();
            let pattern = term.pattern.as_ref().unwrap();
            if pattern.kind == PatternKind::Re {
                for (sid, sterm, spattern_parsed) in &order {
                    let spattern = sterm.pattern.as_ref().unwrap();
                    if spattern.kind != PatternKind::Str || sterm.priority != term.priority {
                        continue;
                    }
                    let text: Vec<char> = spattern.value.chars().collect();
                    if tpattern.match_at(&text, 0) == Ok(Some(text.len())) {
                        unless.push((*sid, spattern_parsed.clone()));
                        if is_subset(&spattern.flags, &pattern.flags) {
                            embedded.push(*sid);
                        }
                    }
                }
            }
            unless_lists.push(unless);
        }
        let terminals: Vec<LexTerminal> = order
            .into_iter()
            .zip(unless_lists)
            .filter(|((id, _, _), _)| !embedded.contains(id))
            .map(|((id, _, pattern), unless)| LexTerminal {
                id,
                pattern,
                unless,
            })
            .collect();
        let mut positions = alloc::vec![None; patterns.len()];
        for (i, term) in terminals.iter().enumerate() {
            positions[term.id] = Some(i);
        }
        Lexer {
            terminals,
            patterns,
            positions,
        }
    }

    /// Match terminal `id`'s own pattern at `pos`, as the dynamic lexer
    /// does for a terminal the parser expects there; a terminal without
    /// a pattern (one `%declare`d) cannot be checked.
    pub fn match_terminal(&self, id: TermId, text: &[char], pos: usize) -> Lexed {
        match &self.patterns[id] {
            Some(pattern) => lexed(id, &pattern.match_at(text, pos)),
            None => Lexed::Unknown,
        }
    }

    /// [`match_terminal`](Self::match_terminal), with how the result
    /// depended on the end of the text.
    pub fn match_terminal_scan(&self, id: TermId, text: &[char], pos: usize) -> Scan {
        let Some(pattern) = &self.patterns[id] else {
            return Scan::new(Lexed::Unknown);
        };
        let outcome = pattern.scan(text, pos);
        let mut scan = Scan::new(lexed(id, &outcome.result));
        scan.note(id, &outcome);
        scan
    }

    /// Whether a token of `candidate` right after a token whose own
    /// scan is `scan` could end that token's match where it is: false
    /// only when the match is known to run on through every character
    /// the candidate can start with, so that it would read on into the
    /// candidate and past it just the same.
    pub fn can_stop(&self, scan: &Scan, candidate: TermId) -> bool {
        let Some(first) = self.patterns[candidate]
            .as_ref()
            .and_then(|pattern| pattern.first.as_ref())
        else {
            return true;
        };
        if first.len() > STOPPERS_TRIED || !scan.carried {
            return true;
        }
        scan.steps.as_slice().iter().all(|(id, carry)| {
            self.patterns[*id]
                .as_ref()
                .is_none_or(|pattern| match carry {
                    Carry::Tail(run_id) => first
                        .chars()
                        .any(|c| pattern.run_len(*run_id, &[c], 0) != Some(1)),
                    Carry::Unless(probes) => first
                        .chars()
                        .any(|c| pattern.probed(probes, c) != Some(true)),
                    Carry::Resume(resume) => pattern
                        .program
                        .as_ref()
                        .is_none_or(|program| first.chars().any(|c| !program.swallows(resume, c))),
                    Carry::Literal => true,
                })
        })
    }

    /// Lex one token of `text` at `pos`, as Lark's basic lexer would.
    pub fn next_token(&self, text: &[char], pos: usize) -> Lexed {
        self.next_token_scan(text, pos).lexed
    }

    /// [`next_token`](Self::next_token), with how the result depended on
    /// the end of the text: a terminal that failed after looking at the
    /// end might match with more text, so the result then stands only for
    /// this text.
    pub fn next_token_scan(&self, text: &[char], pos: usize) -> Scan {
        self.basic_scan(text, pos, None)
    }

    /// [`next_token_scan`](Self::next_token_scan), given `own`, what
    /// [`match_terminal_scan`](Self::match_terminal_scan) found for the
    /// terminal `id` at `pos`, so that its pattern need not be run
    /// again.
    pub fn next_token_scan_knowing(
        &self,
        text: &[char],
        pos: usize,
        id: TermId,
        own: &Scan,
    ) -> Scan {
        self.basic_scan(text, pos, Some((id, own)))
    }

    fn basic_scan(&self, text: &[char], pos: usize, known: Option<(TermId, &Scan)>) -> Scan {
        let mut scan = Scan::new(Lexed::NoMatch);
        if pos >= text.len() {
            scan.carried = false;
            return scan;
        }
        for term in &self.terminals {
            let result = match known {
                Some((id, own)) if id == term.id => {
                    scan.anchored |= own.anchored;
                    scan.carried &= own.carried;
                    scan.steps.extend_from(&own.steps);
                    match own.lexed {
                        Lexed::Token(_, end) => Ok(Some(end)),
                        Lexed::NoMatch => Ok(None),
                        Lexed::Unknown => Err(()),
                    }
                }
                _ if !term.pattern.may_start(text[pos]) => continue,
                _ => {
                    let outcome = term.pattern.scan(text, pos);
                    scan.note(term.id, &outcome);
                    outcome.result.map_err(|_| ())
                }
            };
            match result {
                Ok(Some(end)) => {
                    scan.lexed = Lexed::Token(self.retype(term, &text[pos..end]), end);
                    return scan;
                }
                Ok(None) => {}
                Err(()) => {
                    scan.lexed = Lexed::Unknown;
                    scan.carried = false;
                    return scan;
                }
            }
        }
        scan
    }

    /// The terminal the basic lexer reports a token of `term` with the
    /// text `token` as: a string terminal the text matches completely,
    /// if any, else `term` itself.
    fn retype(&self, term: &LexTerminal, token: &[char]) -> TermId {
        for (sid, spattern) in &term.unless {
            if spattern.fullmatch(token) == Ok(true) {
                return *sid;
            }
        }
        term.id
    }

    /// What `scan`, found at the start of a text of `len` characters,
    /// says about the text that begins the same way at `pos` of `text`:
    /// the first terminal whose outcome the end of the text left open
    /// and that matches on the longer one (re-typed as a keyword, for a
    /// basic-lexer scan, when `retype`), else the same thing shifted to
    /// `pos`; nothing when the finding cannot be carried over.
    pub fn extend(
        &self,
        scan: &Scan,
        text: &[char],
        pos: usize,
        len: usize,
        retype: bool,
    ) -> Option<Lexed> {
        self.carry(scan, text, pos, len, retype, false)
    }

    /// [`extend`](Self::extend), for a `text` that may itself go on past
    /// its end: nothing when the answer could still change with what
    /// follows, so that an answer stands for the token in any text that
    /// continues this one.
    pub fn settle(
        &self,
        scan: &Scan,
        text: &[char],
        pos: usize,
        len: usize,
        retype: bool,
    ) -> Option<Lexed> {
        self.carry(scan, text, pos, len, retype, true)
    }

    fn carry(
        &self,
        scan: &Scan,
        text: &[char],
        pos: usize,
        len: usize,
        retype: bool,
        open: bool,
    ) -> Option<Lexed> {
        if !scan.carried {
            return None;
        }
        for (id, carry) in scan.steps.as_slice() {
            let pattern = self.patterns[*id].as_ref()?;
            let end = match carry {
                Carry::Literal => {
                    let literal = pattern.literal.as_ref()?;
                    let rest = &text[pos..];
                    if open && rest.len() < literal.len() && literal.starts_with(rest) {
                        return None;
                    }
                    rest.starts_with(literal).then(|| pos + literal.len())
                }
                Carry::Tail(run_id) => {
                    let end = pos + len + pattern.run_len(*run_id, text, pos + len)?;
                    if open && end == text.len() {
                        return None;
                    }
                    Some(end)
                }
                Carry::Unless(probes) => match text.get(pos + len) {
                    Some(&c) if pattern.probed(probes, c)? => return None,
                    Some(_) => None,
                    None if open => return None,
                    None => None,
                },
                Carry::Resume(resume) => {
                    let (end, touch) = pattern.program.as_ref()?.resume(resume, text, pos, len);
                    if open && touch != Touch::Never {
                        return None;
                    }
                    end
                }
            };
            if let Some(end) = end {
                let id = if retype {
                    self.retype(&self.terminals[self.positions[*id]?], &text[pos..end])
                } else {
                    *id
                };
                return Some(Lexed::Token(id, end));
            }
        }
        Some(match scan.lexed {
            Lexed::Token(id, end) => Lexed::Token(id, pos + end),
            other => other,
        })
    }
}

/// Which of a list of terminals a match could start with each ASCII
/// character, so that a walk trying the whole list at a position skips
/// the ones that cannot without asking each pattern (see
/// [`TermPattern::may_start`]): bit `n` of a character's word stands for
/// the `n`th terminal of the list, and the last bit for every terminal
/// from the 64th on together.
pub struct Starters {
    ascii: Box<[u64; 128]>,
}

impl Starters {
    /// The starters of the terminals `ids` under `lexer`; a terminal
    /// without a pattern could start anywhere.
    pub fn new(lexer: &Lexer, ids: &[TermId]) -> Starters {
        let mut ascii = Box::new([0u64; 128]);
        for (n, &id) in ids.iter().enumerate() {
            let bit = 1u64 << n.min(63);
            for (c, word) in ascii.iter_mut().enumerate() {
                let c = char::from(c as u8);
                if lexer.patterns[id]
                    .as_ref()
                    .is_none_or(|pattern| pattern.may_start(c))
                {
                    *word |= bit;
                }
            }
        }
        Starters { ascii }
    }

    /// The terminals that could match starting at `c`, as a bit for each
    /// (see [`Starters::has`]); every one of them for a character beyond
    /// ASCII.
    #[inline]
    pub fn at(&self, c: char) -> u64 {
        self.ascii.get(c as usize).copied().unwrap_or(u64::MAX)
    }

    /// Whether the `n`th terminal is among the `word` [`at`](Self::at) a
    /// character.
    #[inline]
    pub fn has(word: u64, n: usize) -> bool {
        word >> n.min(63) & 1 == 1
    }
}

#[cfg(test)]
#[path = "../../../tests/embedded/native/lark/lexsim_tests.rs"]
mod tests;
