//! Backtracking matcher for the pattern AST produced by [`super::parser`],
//! replicating CPython's `_sre` engine closely enough that a match ends
//! where CPython's `pattern.match(text, pos)` would end: alternatives are
//! tried left to right and the first that lets the rest of the pattern
//! succeed wins, greedy repeats give back one item at a time, lazy repeats
//! take one more item at a time, and possessive repeats and atomic groups
//! never give anything back.
//!
//! The grammar generator uses it to check that a generated terminal lexes
//! back to itself (Hypothesis's `lexes_entirely` filter) and to simulate a
//! lexer at the boundary between two generated tokens, which needs
//! CPython's leftmost-first semantics rather than leftmost-longest.
//!
//! Text is indexed by codepoint, so callers pass `&[char]`. Matching is a
//! recursive descent with continuation passing; a step budget bounds the
//! work on patterns with catastrophic backtracking and a depth budget
//! bounds the Rust stack a repeat of a compound item can consume.

use alloc::vec::Vec;

use super::casefold;
use super::categories;
use super::constants::{
    AtCode, ChCode, MAXREPEAT, SRE_FLAG_ASCII, SRE_FLAG_DOTALL, SRE_FLAG_IGNORECASE,
    SRE_FLAG_MULTILINE, TYPE_FLAGS, unicode_version,
};
use super::parser::{OpCode, ParsedPattern, SetItem, SubPattern};
use super::width::get_width_in;

/// The default number of matching steps one match may take before it is
/// abandoned with [`MatchError::StepLimit`].
pub const DEFAULT_STEP_LIMIT: u64 = 1_000_000;

/// The default nesting depth of pattern sequences one match may reach
/// before it is abandoned with [`MatchError::DepthLimit`]. Only repeats of
/// items with capture groups, assertions or nested repeats recurse per
/// iteration (several levels each); repeats of single characters and of
/// alternations of plain character sequences run on an explicit stack,
/// and a string checked against a pattern as its constraint goes to the
/// simulation in [`vm`](super::vm) for every pattern that can express,
/// so the limit bears on patterns with backreferences, conditional
/// groups, atomic groups or possessive repeats, and on the lexer
/// simulation's own matches. A level takes up to about 1.5 KiB of stack
/// in an unoptimised build (about 0.5 KiB at opt-level 1), so this
/// keeps a match within 600 KiB: inside a 1 MiB thread stack, the
/// smallest a thread commonly gets, with room to spare.
pub const DEFAULT_DEPTH_LIMIT: usize = 400;

/// Why a match was abandoned without an answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchError {
    /// The step budget ran out (catastrophic backtracking).
    StepLimit,
    /// The recursion budget ran out (a long repeat of a compound item).
    DepthLimit,
}

impl core::fmt::Display for MatchError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            MatchError::StepLimit => write!(f, "regex match exceeded its step limit"),
            MatchError::DepthLimit => write!(f, "regex match exceeded its nesting depth limit"),
        }
    }
}

type MatchResult = Result<bool, MatchError>;

/// A matcher for one parsed pattern.
pub struct Matcher<'p> {
    pattern: &'p SubPattern,
    flags: u32,
    step_limit: u64,
    depth_limit: usize,
    /// Whether the rest of a repeat can be assumed to match the same way
    /// from the same position however the repeat got there: true unless
    /// the pattern has backreferences or conditional groups, whose
    /// matching depends on what was captured on the way.
    memoize: bool,
}

/// The positions at which the continuation of one repeat iteration has
/// already failed, so that reaching them again by another path through
/// the iteration (or, for the run of a single unit, by giving back a
/// different number of units) need not retry it. Only used when the
/// matcher may memoize; see [`Matcher::memoize`].
struct FailedEnds {
    active: bool,
    ends: Vec<usize>,
}

impl FailedEnds {
    fn new(active: bool) -> FailedEnds {
        FailedEnds {
            active,
            ends: Vec::new(),
        }
    }

    fn contains(&self, end: usize) -> bool {
        self.active && self.ends.contains(&end)
    }

    fn record(&mut self, end: usize, result: &MatchResult) {
        if self.active && matches!(result, Ok(false)) {
            self.ends.push(end);
        }
    }
}

/// How a match depended on the edges of the text, which says whether
/// the same text with more around it (before its start or after its
/// end) would match the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Touch {
    /// The matcher looked at neither edge of the text: the result stands
    /// for the same text with anything before and after it.
    Never,
    /// The only thing that looked at the end was the forward scan of one
    /// unbounded greedy repeat of a single unit (identified by the
    /// `run_id` of [`find_run`]), the match took the whole run and ends
    /// at the end of the text, and nothing after the run read a
    /// character the run covered: on a longer text it ends after the run
    /// of that unit continuing from there.
    Tail(usize),
    /// Only tests of single characters looked at the end, and every one
    /// found no character there: on a longer text whose next character
    /// none of them matches they all fail the same way, so the result
    /// stands.
    Unless(Vec<Probe>),
    /// The result may change with more text.
    End,
}

/// A test of a single character that read the end of the text, as
/// [`probe_matches`] can run it again on the character that follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    Literal(u32, u32),
    NotLiteral(u32, u32),
    Any(u32),
    /// A character class, by the address of its items.
    In(usize, u32),
    /// The unit of the unbounded greedy repeat `run_id` (see
    /// [`find_run`]).
    Run(usize),
}

impl Probe {
    fn new(unit: UnitOp<'_>, flags: u32) -> Probe {
        match unit {
            UnitOp::Literal(cp) => Probe::Literal(cp, flags),
            UnitOp::NotLiteral(cp) => Probe::NotLiteral(cp, flags),
            UnitOp::Any => Probe::Any(flags),
            UnitOp::In(items) => Probe::In(items.as_ptr() as usize, flags),
        }
    }
}

impl Touch {
    /// Record a read at the end of the text by something other than a
    /// single-character test.
    fn end(&mut self) {
        *self = Touch::End;
    }

    /// A cheap summary that changes whenever the touch does: a touch only
    /// ever moves from `Never` through `Tail` and `Unless` to `End`, and
    /// an `Unless` only ever gains probes.
    fn stamp(&self) -> (u8, usize) {
        match self {
            Touch::Never => (0, 0),
            Touch::Tail(run_id) => (1, *run_id),
            Touch::Unless(probes) => (2, probes.len()),
            Touch::End => (3, 0),
        }
    }

    /// Record a single-character test finding the end of the text.
    fn tested(&mut self, probe: Probe) {
        *self = match core::mem::replace(self, Touch::End) {
            Touch::Never => Touch::Unless(alloc::vec![probe]),
            Touch::Tail(run_id) => Touch::Unless(alloc::vec![Probe::Run(run_id), probe]),
            Touch::Unless(mut probes) => {
                if !probes.contains(&probe) {
                    probes.push(probe);
                }
                Touch::Unless(probes)
            }
            Touch::End => Touch::End,
        };
    }

    /// Record that the match no longer follows the run whose forward
    /// scan reached the end: the rest of the pattern failed after the
    /// whole run, or read a character the run covered (a lookbehind, a
    /// backreference, a multiline `^`), which a longer run would have
    /// moved. A longer text then matches the same way only when the run
    /// does not go on.
    fn untail(&mut self) {
        if let Touch::Tail(run_id) = *self {
            *self = Touch::Unless(alloc::vec![Probe::Run(run_id)]);
        }
    }

    /// Record the forward scan of the unbounded greedy unit repeat
    /// `run_id` reaching the end of the text.
    fn tail(&mut self, run_id: usize) {
        *self = match core::mem::replace(self, Touch::End) {
            Touch::Never => Touch::Tail(run_id),
            Touch::Tail(id) if id == run_id => Touch::Tail(id),
            Touch::Tail(id) => Touch::Unless(alloc::vec![Probe::Run(id), Probe::Run(run_id)]),
            Touch::Unless(mut probes) => {
                if !probes.contains(&Probe::Run(run_id)) {
                    probes.push(Probe::Run(run_id));
                }
                Touch::Unless(probes)
            }
            Touch::End => Touch::End,
        };
    }
}

/// Per-match mutable state: the text, the capture groups set so far, and
/// the remaining budgets.
struct State<'t> {
    text: &'t [char],
    touch: Touch,
    marks: Vec<Option<(usize, usize)>>,
    /// Whether the groups' marks are kept at all: nothing reads them
    /// unless the pattern refers back to a group.
    track_marks: bool,
    /// The last run of a single unit measured for an unbounded greedy
    /// repeat, as (the repeat's identity, run start, run end): a repeat
    /// re-entered anywhere inside that run ends at the same place.
    run: Option<(usize, usize, usize)>,
    steps: u64,
    step_limit: u64,
    depth: usize,
    depth_limit: usize,
}

impl State<'_> {
    fn step(&mut self) -> Result<(), MatchError> {
        self.steps += 1;
        if self.steps > self.step_limit {
            return Err(MatchError::StepLimit);
        }
        Ok(())
    }

    fn enter(&mut self) -> Result<(), MatchError> {
        self.depth += 1;
        if self.depth > self.depth_limit {
            return Err(MatchError::DepthLimit);
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    fn group(&self, g: u32) -> Option<(usize, usize)> {
        self.marks.get(g as usize).copied().flatten()
    }

    /// Whether the character at `index` matches `unit`, noting a read at
    /// the end of the text.
    fn unit_at(&mut self, index: usize, unit: UnitOp<'_>, flags: u32) -> bool {
        match self.text.get(index) {
            Some(&c) => unit_matches(unit, c, flags),
            None => {
                self.touch.tested(Probe::new(unit, flags));
                false
            }
        }
    }

    /// [`at_matches`] at `pos`, noting when the verdict depends on where
    /// the text ends, and that `^` reads the character before `pos`.
    fn at(&mut self, at: AtCode, pos: usize, flags: u32) -> bool {
        if pos == 0
            || (!matches!(at, AtCode::Beginning | AtCode::BeginningString)
                && pos + 1 >= self.text.len())
        {
            self.touch.end();
        } else if at == AtCode::Beginning {
            self.touch.untail();
        }
        at_matches(at, self.text, pos, flags)
    }

    fn set_group(&mut self, g: u32, value: Option<(usize, usize)>) {
        let g = g as usize;
        if !self.track_marks {
            return;
        }
        if self.marks.len() <= g {
            self.marks.resize(g + 1, None);
        }
        self.marks[g] = value;
    }

    /// The marks as they are, to put back with [`State::restore_marks`]
    /// after a failed attempt.
    fn save_marks(&self) -> Vec<Option<(usize, usize)>> {
        if self.track_marks {
            self.marks.clone()
        } else {
            Vec::new()
        }
    }

    fn restore_marks(&mut self, saved: Vec<Option<(usize, usize)>>) {
        if self.track_marks {
            self.marks = saved;
        }
    }
}

/// A continuation: what must match after the current construct, starting
/// at the given position.
type Cont<'a, 't> = &'a mut dyn FnMut(&mut State<'t>, usize) -> MatchResult;

/// A single-character matching op, the items CPython compiles to
/// `REPEAT_ONE` / `MIN_REPEAT_ONE` so a repeat of them runs as a counted
/// loop rather than one recursion per iteration.
#[derive(Clone, Copy)]
enum UnitOp<'a> {
    Literal(u32),
    NotLiteral(u32),
    Any,
    In(&'a [SetItem]),
}

fn as_unit(op: &OpCode) -> Option<UnitOp<'_>> {
    match op {
        OpCode::Literal(cp) => Some(UnitOp::Literal(*cp)),
        OpCode::NotLiteral(cp) => Some(UnitOp::NotLiteral(*cp)),
        OpCode::Any => Some(UnitOp::Any),
        OpCode::In(items) => Some(UnitOp::In(items)),
        _ => None,
    }
}

/// CPython's `_simple`: a one-op pattern that is a unit, or a
/// non-capturing group around one, with the flags in force inside it.
fn single_unit(p: &SubPattern, flags: u32) -> Option<(UnitOp<'_>, u32)> {
    match p.data.as_slice() {
        [
            OpCode::Subpattern {
                group: None,
                add_flags,
                del_flags,
                p,
            },
        ] => single_unit(p, combine_flags(flags, *add_flags, *del_flags)),
        [op] => as_unit(op).map(|unit| (unit, flags)),
        _ => None,
    }
}

/// One alternative of a repeat item handled on the explicit stack: a
/// sequence of units, each with the flags in force for it.
type UnitSequence<'a> = Vec<(UnitOp<'a>, u32)>;

/// Collect the alternatives of `p` into `out` when `p` is a sequence of
/// units or an alternation (possibly nested in non-capturing groups) of
/// such sequences, the shape of most grammar terminals' repeats. Returns
/// false, leaving `out` unspecified, when `p` contains anything else.
fn unit_alternatives<'a>(p: &'a SubPattern, flags: u32, out: &mut Vec<UnitSequence<'a>>) -> bool {
    match p.data.as_slice() {
        [
            OpCode::Subpattern {
                group: None,
                add_flags,
                del_flags,
                p,
            },
        ] => unit_alternatives(p, combine_flags(flags, *add_flags, *del_flags), out),
        [OpCode::Branch(alternatives)] => alternatives
            .iter()
            .all(|alternative| unit_alternatives(alternative, flags, out)),
        ops => {
            if !ops.iter().all(|op| as_unit(op).is_some()) {
                return false;
            }
            out.push(
                ops.iter()
                    .filter_map(as_unit)
                    .map(|unit| (unit, flags))
                    .collect(),
            );
            true
        }
    }
}

/// The end of `sequence` matched at `pos`, if it matches there.
fn sequence_end(st: &mut State<'_>, pos: usize, sequence: &UnitSequence<'_>) -> Option<usize> {
    for (offset, &(unit, flags)) in sequence.iter().enumerate() {
        if !st.unit_at(pos + offset, unit, flags) {
            return None;
        }
    }
    Some(pos + sequence.len())
}

/// The first alternative at index `from` or later that matches at `pos`,
/// with its end.
fn first_alternative(
    st: &mut State<'_>,
    alternatives: &[UnitSequence<'_>],
    pos: usize,
    from: usize,
) -> Option<(usize, usize)> {
    alternatives
        .iter()
        .enumerate()
        .skip(from)
        .find_map(|(index, alternative)| sequence_end(st, pos, alternative).map(|end| (index, end)))
}

/// CPython's `_combine_flags`: a scoped type flag (`a`/`u`/`L`) replaces
/// the inherited one rather than adding to it.
pub(crate) fn combine_flags(mut flags: u32, add_flags: u32, del_flags: u32) -> u32 {
    if add_flags & TYPE_FLAGS != 0 {
        flags &= !TYPE_FLAGS;
    }
    (flags | add_flags) & !del_flags
}

impl<'p> Matcher<'p> {
    /// A matcher for `parsed` with the default budgets.
    pub fn new(parsed: &'p ParsedPattern) -> Self {
        Matcher {
            pattern: &parsed.pattern,
            flags: parsed.flags,
            step_limit: DEFAULT_STEP_LIMIT,
            depth_limit: DEFAULT_DEPTH_LIMIT,
            memoize: !parsed.group_refs,
        }
    }

    /// Replace the step and depth budgets.
    #[cfg(test)]
    pub fn with_limits(mut self, step_limit: u64, depth_limit: usize) -> Self {
        self.step_limit = step_limit;
        self.depth_limit = depth_limit;
        self
    }

    fn state<'t>(&self, text: &'t [char]) -> State<'t> {
        State {
            text,
            touch: Touch::Never,
            marks: Vec::new(),
            track_marks: !self.memoize,
            run: None,
            steps: 0,
            step_limit: self.step_limit,
            depth: 0,
            depth_limit: self.depth_limit,
        }
    }

    /// The end of the match CPython's `pattern.match(text, start)` finds,
    /// or `None` when the pattern does not match at `start` (a `start`
    /// past the end of the text is clamped to it, as CPython does), with
    /// how the match depended on the edges of the text.
    pub fn match_at_touch(
        &self,
        text: &[char],
        start: usize,
    ) -> Result<(Option<usize>, Touch), MatchError> {
        let start = start.min(text.len());
        let mut state = self.state(text);
        let mut end = None;
        let matched = self.seq(
            &mut state,
            &self.pattern.data,
            start,
            self.flags,
            &mut |_, pos| {
                end = Some(pos);
                Ok(true)
            },
        );
        let matched = matched?;
        let end = if matched { end } else { None };
        Ok((end, state.touch))
    }

    /// Whether the pattern matches the whole of `text`, like CPython's
    /// `pattern.fullmatch(text)`: unlike [`match_at_touch`](Self::match_at_touch)
    /// reaching the end, this backtracks into the pattern looking for a
    /// match that ends exactly at the end.
    pub fn fullmatch(&self, text: &[char]) -> Result<bool, MatchError> {
        let mut state = self.state(text);
        self.seq(
            &mut state,
            &self.pattern.data,
            0,
            self.flags,
            &mut |_, pos| Ok(pos == text.len()),
        )
    }

    /// Match the sequence `ops` at `pos`, then the continuation `k`.
    /// Single-character ops and position assertions advance in a loop;
    /// everything else hands the rest of the sequence to
    /// [`compound`](Self::compound). A call that returns `Ok(false)` leaves
    /// the capture groups as it found them.
    fn seq<'t>(
        &self,
        st: &mut State<'t>,
        ops: &[OpCode],
        pos: usize,
        flags: u32,
        k: Cont<'_, 't>,
    ) -> MatchResult {
        st.enter()?;
        let result = self.seq_body(st, ops, pos, flags, k);
        st.leave();
        result
    }

    fn seq_body<'t>(
        &self,
        st: &mut State<'t>,
        ops: &[OpCode],
        mut pos: usize,
        flags: u32,
        k: Cont<'_, 't>,
    ) -> MatchResult {
        let mut index = 0;
        loop {
            st.step()?;
            let Some(op) = ops.get(index) else {
                return k(st, pos);
            };
            if let Some(unit) = as_unit(op) {
                if !st.unit_at(pos, unit, flags) {
                    return Ok(false);
                }
                pos += 1;
                index += 1;
                continue;
            }
            match op {
                OpCode::At(at) => {
                    if !st.at(*at, pos, flags) {
                        return Ok(false);
                    }
                    index += 1;
                }
                OpCode::Failure => return Ok(false),
                _ => return self.compound(st, op, &ops[index + 1..], pos, flags, k),
            }
        }
    }

    /// Match one compound op at `pos`, then `rest`, then `k`.
    fn compound<'t>(
        &self,
        st: &mut State<'t>,
        op: &OpCode,
        rest: &[OpCode],
        pos: usize,
        flags: u32,
        k: Cont<'_, 't>,
    ) -> MatchResult {
        match op {
            OpCode::Branch(alternatives) => {
                for alternative in alternatives {
                    if self.seq(st, &alternative.data, pos, flags, &mut |st, end| {
                        self.seq(st, rest, end, flags, k)
                    })? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            OpCode::Subpattern {
                group,
                add_flags,
                del_flags,
                p,
            } => {
                let inner = combine_flags(flags, *add_flags, *del_flags);
                match *group {
                    None => self.seq(st, &p.data, pos, inner, &mut |st, end| {
                        self.seq(st, rest, end, flags, k)
                    }),
                    Some(g) => self.seq(st, &p.data, pos, inner, &mut |st, end| {
                        let saved = st.group(g);
                        st.set_group(g, Some((pos, end)));
                        let result = self.seq(st, rest, end, flags, k);
                        if !matches!(result, Ok(true)) {
                            st.set_group(g, saved);
                        }
                        result
                    }),
                }
            }
            OpCode::GroupRef(g) => {
                let Some((start, end)) = st.group(*g) else {
                    return Ok(false);
                };
                st.touch.untail();
                let len = end - start;
                if pos + len > st.text.len() {
                    st.touch.end();
                    return Ok(false);
                }
                for offset in 0..len {
                    if !casefold::groupref_eq(st.text[start + offset], st.text[pos + offset], flags)
                    {
                        return Ok(false);
                    }
                }
                self.seq(st, rest, pos + len, flags, k)
            }
            OpCode::GroupRefExists {
                cond_group,
                yes,
                no,
            } => {
                if st.group(*cond_group).is_some() {
                    self.seq(st, &yes.data, pos, flags, &mut |st, end| {
                        self.seq(st, rest, end, flags, k)
                    })
                } else if let Some(no) = no {
                    self.seq(st, &no.data, pos, flags, &mut |st, end| {
                        self.seq(st, rest, end, flags, k)
                    })
                } else {
                    self.seq(st, rest, pos, flags, k)
                }
            }
            OpCode::Assert { direction, p } => {
                let saved = st.save_marks();
                if !self.lookaround(st, *direction, p, pos, flags)? {
                    return Ok(false);
                }
                if self.seq(st, rest, pos, flags, k)? {
                    return Ok(true);
                }
                st.restore_marks(saved);
                Ok(false)
            }
            OpCode::AssertNot { direction, p } => {
                let saved = st.save_marks();
                if self.lookaround(st, *direction, p, pos, flags)? {
                    st.restore_marks(saved);
                    return Ok(false);
                }
                self.seq(st, rest, pos, flags, k)
            }
            OpCode::AtomicGroup(p) => {
                let saved = st.save_marks();
                let Some(end) = self.first_end(st, p, pos, flags)? else {
                    return Ok(false);
                };
                if self.seq(st, rest, end, flags, k)? {
                    return Ok(true);
                }
                st.touch.untail();
                st.restore_marks(saved);
                Ok(false)
            }
            OpCode::MaxRepeat { min, max, item } => {
                if let Some((unit, unit_flags)) = single_unit(item, flags) {
                    let run_id = core::ptr::from_ref(item) as usize;
                    return self.greedy_unit(
                        st, run_id, unit, unit_flags, *min, *max, rest, pos, flags, k,
                    );
                }
                let mut alternatives = Vec::new();
                if unit_alternatives(item, flags, &mut alternatives) {
                    return self.greedy_sequences(
                        st,
                        &alternatives,
                        *min,
                        *max,
                        rest,
                        pos,
                        flags,
                        k,
                    );
                }
                self.greedy(st, item, *min, *max, 0, None, rest, pos, flags, k)
            }
            OpCode::MinRepeat { min, max, item } => {
                if let Some((unit, unit_flags)) = single_unit(item, flags) {
                    return self.lazy_unit(st, unit, unit_flags, *min, *max, rest, pos, flags, k);
                }
                let mut alternatives = Vec::new();
                if unit_alternatives(item, flags, &mut alternatives) {
                    return self.lazy_sequences(st, &alternatives, *min, *max, rest, pos, flags, k);
                }
                self.lazy(st, item, *min, *max, 0, None, rest, pos, flags, k)
            }
            OpCode::PossessiveRepeat { min, max, item } => {
                self.possessive(st, item, *min, *max, rest, pos, flags, k)
            }
            OpCode::Literal(_)
            | OpCode::NotLiteral(_)
            | OpCode::Any
            | OpCode::In(_)
            | OpCode::At(_)
            | OpCode::Failure => unreachable!("compound: {:?} is handled by seq_body", op),
        }
    }

    /// The end of the first match of `p` at `pos` in CPython's order, with
    /// no backtracking into it afterwards: the body of an atomic group, a
    /// possessive iteration, or a lookahead.
    fn first_end<'t>(
        &self,
        st: &mut State<'t>,
        p: &SubPattern,
        pos: usize,
        flags: u32,
    ) -> Result<Option<usize>, MatchError> {
        let mut end = None;
        let matched = self.seq(st, &p.data, pos, flags, &mut |_, e| {
            end = Some(e);
            Ok(true)
        })?;
        Ok(if matched { end } else { None })
    }

    /// Whether the lookaround body `p` matches at `pos`: ahead of it for a
    /// positive `direction`, ending exactly at it for a negative one,
    /// having started its fixed width before (the parser rejects a
    /// lookbehind of any other kind, as CPython does).
    fn lookaround<'t>(
        &self,
        st: &mut State<'t>,
        direction: i32,
        p: &SubPattern,
        pos: usize,
        flags: u32,
    ) -> MatchResult {
        if direction > 0 {
            let before = st.touch.stamp();
            let found = self.first_end(st, p, pos, flags)?.is_some();
            if st.touch.stamp() != before {
                st.touch.end();
            }
            return Ok(found);
        }
        let w = usize::try_from(get_width_in(p, self.pattern).0).unwrap_or(usize::MAX);
        if w > pos {
            st.touch.end();
            return Ok(false);
        }
        if w > 0 {
            st.touch.untail();
        }
        self.seq(st, &p.data, pos - w, flags, &mut |_, end| Ok(end == pos))
    }

    /// CPython's `MAX_UNTIL`: `count` iterations of `item` are done; take
    /// another if allowed and the previous one consumed something, else
    /// fall through to the rest.
    fn greedy<'t>(
        &self,
        st: &mut State<'t>,
        item: &SubPattern,
        min: u32,
        max: u32,
        count: u32,
        last_pos: Option<usize>,
        rest: &[OpCode],
        pos: usize,
        flags: u32,
        k: Cont<'_, 't>,
    ) -> MatchResult {
        st.step()?;
        let mut failed = FailedEnds::new(self.memoize);
        if count < min {
            return self.seq(st, &item.data, pos, flags, &mut |st, end| {
                if failed.contains(end) {
                    return Ok(false);
                }
                let result =
                    self.greedy(st, item, min, max, count + 1, last_pos, rest, end, flags, k);
                failed.record(end, &result);
                result
            });
        }
        if (max == MAXREPEAT || count < max)
            && last_pos != Some(pos)
            && self.seq(st, &item.data, pos, flags, &mut |st, end| {
                if failed.contains(end) {
                    return Ok(false);
                }
                let result = self.greedy(
                    st,
                    item,
                    min,
                    max,
                    count + 1,
                    Some(pos),
                    rest,
                    end,
                    flags,
                    k,
                );
                failed.record(end, &result);
                result
            })?
        {
            return Ok(true);
        }
        self.seq(st, rest, pos, flags, k)
    }

    /// CPython's `MIN_UNTIL`: once `min` iterations are done, try the rest
    /// first and take another iteration only if that fails.
    fn lazy<'t>(
        &self,
        st: &mut State<'t>,
        item: &SubPattern,
        min: u32,
        max: u32,
        count: u32,
        last_pos: Option<usize>,
        rest: &[OpCode],
        pos: usize,
        flags: u32,
        k: Cont<'_, 't>,
    ) -> MatchResult {
        st.step()?;
        let mut failed = FailedEnds::new(self.memoize);
        if count < min {
            return self.seq(st, &item.data, pos, flags, &mut |st, end| {
                if failed.contains(end) {
                    return Ok(false);
                }
                let result =
                    self.lazy(st, item, min, max, count + 1, last_pos, rest, end, flags, k);
                failed.record(end, &result);
                result
            });
        }
        if self.seq(st, rest, pos, flags, k)? {
            return Ok(true);
        }
        if (max != MAXREPEAT && count >= max) || last_pos == Some(pos) {
            return Ok(false);
        }
        self.seq(st, &item.data, pos, flags, &mut |st, end| {
            if failed.contains(end) {
                return Ok(false);
            }
            let result = self.lazy(
                st,
                item,
                min,
                max,
                count + 1,
                Some(pos),
                rest,
                end,
                flags,
                k,
            );
            failed.record(end, &result);
            result
        })
    }

    /// CPython's `REPEAT_ONE`: count the most units that match, then give
    /// them back one at a time. A scan that reaches the end of the text
    /// records a [`Touch::Tail`], which stands only while the match
    /// follows the whole run: once the rest fails after it, the touch
    /// is downgraded.
    fn greedy_unit<'t>(
        &self,
        st: &mut State<'t>,
        run_id: usize,
        unit: UnitOp<'_>,
        unit_flags: u32,
        min: u32,
        max: u32,
        rest: &[OpCode],
        pos: usize,
        flags: u32,
        k: Cont<'_, 't>,
    ) -> MatchResult {
        let limit = repeat_limit(max);
        let mut count = match st.run {
            Some((id, start, end)) if id == run_id && (start..=end).contains(&pos) => end - pos,
            _ => 0,
        };
        while count < limit && pos + count < st.text.len() {
            if !unit_matches(unit, st.text[pos + count], unit_flags) {
                break;
            }
            count += 1;
        }
        if count < limit && pos + count == st.text.len() {
            if max == MAXREPEAT {
                st.touch.tail(run_id);
            } else {
                st.touch.tested(Probe::new(unit, unit_flags));
            }
        }
        if max == MAXREPEAT {
            st.run = Some((run_id, pos, pos + count));
        }
        if count < min as usize {
            st.touch.untail();
            return Ok(false);
        }
        loop {
            st.step()?;
            if self.seq(st, rest, pos + count, flags, k)? {
                return Ok(true);
            }
            st.touch.untail();
            if count == min as usize {
                return Ok(false);
            }
            count -= 1;
        }
    }

    /// CPython's `MIN_REPEAT_ONE`: take the minimum, then one more unit
    /// each time the rest fails.
    fn lazy_unit<'t>(
        &self,
        st: &mut State<'t>,
        unit: UnitOp<'_>,
        unit_flags: u32,
        min: u32,
        max: u32,
        rest: &[OpCode],
        pos: usize,
        flags: u32,
        k: Cont<'_, 't>,
    ) -> MatchResult {
        let limit = repeat_limit(max);
        let mut count = 0;
        while count < min as usize {
            if !st.unit_at(pos + count, unit, unit_flags) {
                return Ok(false);
            }
            count += 1;
        }
        loop {
            st.step()?;
            if self.seq(st, rest, pos + count, flags, k)? {
                return Ok(true);
            }
            if count >= limit || !st.unit_at(pos + count, unit, unit_flags) {
                return Ok(false);
            }
            count += 1;
        }
    }

    /// [`greedy`](Self::greedy) for an item that is an alternation of unit
    /// sequences, on an explicit stack of `(iteration start, alternative)`
    /// frames so that long repeats cost heap rather than Rust stack. The
    /// order is CPython's: at each iteration try the alternatives in
    /// order, each followed by further iterations, and only when all fail
    /// try the rest of the pattern at the iteration's start.
    fn greedy_sequences<'t>(
        &self,
        st: &mut State<'t>,
        alternatives: &[UnitSequence<'_>],
        min: u32,
        max: u32,
        rest: &[OpCode],
        pos: usize,
        flags: u32,
        k: Cont<'_, 't>,
    ) -> MatchResult {
        let min = min as usize;
        let mut frames: Vec<(usize, usize)> = Vec::new();
        let mut cur = pos;
        loop {
            while repeat_allowed(&frames, min, max, cur) {
                st.step()?;
                match first_alternative(st, alternatives, cur, 0) {
                    Some((index, end)) => {
                        frames.push((cur, index));
                        cur = end;
                    }
                    None => break,
                }
            }
            if frames.len() >= min && self.seq(st, rest, cur, flags, k)? {
                return Ok(true);
            }
            loop {
                let Some((start, index)) = frames.pop() else {
                    return Ok(false);
                };
                st.step()?;
                if let Some((next, end)) = first_alternative(st, alternatives, start, index + 1) {
                    frames.push((start, next));
                    cur = end;
                    break;
                }
                if frames.len() >= min && self.seq(st, rest, start, flags, k)? {
                    return Ok(true);
                }
            }
        }
    }

    /// [`lazy`](Self::lazy) for an item that is an alternation of unit
    /// sequences, on an explicit stack like
    /// [`greedy_sequences`](Self::greedy_sequences): at each iteration past
    /// the minimum try the rest of the pattern first, then the alternatives
    /// in order.
    fn lazy_sequences<'t>(
        &self,
        st: &mut State<'t>,
        alternatives: &[UnitSequence<'_>],
        min: u32,
        max: u32,
        rest: &[OpCode],
        pos: usize,
        flags: u32,
        k: Cont<'_, 't>,
    ) -> MatchResult {
        let min = min as usize;
        let mut frames: Vec<(usize, usize)> = Vec::new();
        let mut cur = pos;
        loop {
            st.step()?;
            if frames.len() >= min && self.seq(st, rest, cur, flags, k)? {
                return Ok(true);
            }
            if repeat_allowed(&frames, min, max, cur) {
                if let Some((index, end)) = first_alternative(st, alternatives, cur, 0) {
                    frames.push((cur, index));
                    cur = end;
                    continue;
                }
            }
            loop {
                let Some((start, index)) = frames.pop() else {
                    return Ok(false);
                };
                st.step()?;
                if let Some((next, end)) = first_alternative(st, alternatives, start, index + 1) {
                    frames.push((start, next));
                    cur = end;
                    break;
                }
            }
        }
    }

    /// CPython's `POSSESSIVE_REPEAT`: each iteration takes the first match
    /// of `item`, iteration stops at the first failure or empty match, and
    /// nothing is given back when the rest fails.
    fn possessive<'t>(
        &self,
        st: &mut State<'t>,
        item: &SubPattern,
        min: u32,
        max: u32,
        rest: &[OpCode],
        pos: usize,
        flags: u32,
        k: Cont<'_, 't>,
    ) -> MatchResult {
        let saved = st.save_marks();
        let mut count = 0;
        let mut cur = pos;
        while count < min {
            let Some(end) = self.first_end(st, item, cur, flags)? else {
                st.restore_marks(saved);
                return Ok(false);
            };
            cur = end;
            count += 1;
        }
        while max == MAXREPEAT || count < max {
            let Some(end) = self.first_end(st, item, cur, flags)? else {
                break;
            };
            count += 1;
            if end == cur {
                break;
            }
            cur = end;
        }
        if self.seq(st, rest, cur, flags, k)? {
            return Ok(true);
        }
        st.touch.untail();
        st.restore_marks(saved);
        Ok(false)
    }
}

fn repeat_limit(max: u32) -> usize {
    if max == MAXREPEAT {
        usize::MAX
    } else {
        max as usize
    }
}

/// Whether another iteration may start at `cur` given the iterations in
/// `frames`: always below the minimum, and otherwise below the maximum
/// and not at the position the previous post-minimum iteration started
/// from (CPython's `last_ptr` guard against looping on empty matches).
fn repeat_allowed(frames: &[(usize, usize)], min: usize, max: u32, cur: usize) -> bool {
    let count = frames.len();
    if count < min {
        return true;
    }
    if count >= repeat_limit(max) {
        return false;
    }
    let last_start = if count > min {
        frames.last().map(|&(start, _)| start)
    } else {
        None
    };
    last_start != Some(cur)
}

fn unit_matches(op: UnitOp<'_>, c: char, flags: u32) -> bool {
    match op {
        UnitOp::Literal(cp) => literal_eq(cp, c, flags),
        UnitOp::NotLiteral(cp) => !literal_eq(cp, c, flags),
        UnitOp::Any => c != '\n' || flags & SRE_FLAG_DOTALL != 0,
        UnitOp::In(items) => set_matches(items, c, flags),
    }
}

/// The code points a match of `parsed` consists of, with the flags in
/// force for them, when the pattern is a plain sequence of literals,
/// possibly inside non-capturing groups that set flags (the shape of a
/// string terminal with flags, `(?i:KeyWord)`); `None` for any other
/// pattern. Such a pattern fully matches a text exactly when the text is
/// as long as the sequence and each character passes [`literal_eq`]
/// against its code point, which is what [`Matcher::fullmatch`] would
/// find by recursing through the groups.
pub fn literal_sequence(parsed: &ParsedPattern) -> Option<(Vec<u32>, u32)> {
    let mut ops = parsed.pattern.data.as_slice();
    let mut flags = parsed.flags;
    while let [
        OpCode::Subpattern {
            group: None,
            add_flags,
            del_flags,
            p,
        },
    ] = ops
    {
        flags = combine_flags(flags, *add_flags, *del_flags);
        ops = &p.data;
    }
    ops.iter()
        .map(|op| match op {
            OpCode::Literal(cp) => Some(*cp),
            _ => None,
        })
        .collect::<Option<Vec<u32>>>()
        .map(|literals| (literals, flags))
}

/// CPython's `LITERAL` / `LITERAL_IGNORE` / `LITERAL_UNI_IGNORE` test
/// (see [`casefold::literal_matches`]). The parser only emits valid
/// codepoints.
pub fn literal_eq(cp: u32, c: char, flags: u32) -> bool {
    char::from_u32(cp).is_some_and(|l| casefold::literal_matches(l, c, flags))
}

/// Whether the categories and word boundaries take their Unicode
/// definitions under `flags`: every mode but the ASCII flag's.
fn unicode(flags: u32) -> bool {
    flags & SRE_FLAG_ASCII == 0
}

/// CPython's `IN` / `IN_IGNORE` / `IN_UNI_IGNORE` test: a literal or
/// range matches every character case-equal to one of its members under
/// IGNORECASE (see [`casefold`]), and a category is tested against the
/// lowercase form of the character, as CPython tests the whole set.
pub(crate) fn set_matches(items: &[SetItem], c: char, flags: u32) -> bool {
    let negate = matches!(items.first(), Some(SetItem::Negate));
    let probe = if flags & SRE_FLAG_IGNORECASE != 0 {
        casefold::lower(c, flags)
    } else {
        c
    };
    let contained = items.iter().any(|item| match item {
        SetItem::Negate => false,
        SetItem::Literal(cp) => literal_eq(*cp, c, flags),
        SetItem::Range(lo, hi) => casefold::range_matches(*lo, *hi, c, flags),
        SetItem::Category(category) => in_category(probe, *category, flags),
    });
    contained != negate
}

/// CPython's category tests under `flags`: the `UNI_` variants in
/// Unicode mode, for the Unicode version the flags carry, the ASCII ones
/// otherwise.
pub(crate) fn in_category(c: char, category: ChCode, flags: u32) -> bool {
    match category {
        ChCode::Digit => is_digit(c, flags),
        ChCode::NotDigit => !is_digit(c, flags),
        ChCode::Space => is_space(c, flags),
        ChCode::NotSpace => !is_space(c, flags),
        ChCode::Word => is_word(c, flags),
        ChCode::NotWord => !is_word(c, flags),
    }
}

fn is_digit(c: char, flags: u32) -> bool {
    if unicode(flags) && !c.is_ascii() {
        categories::in_class(c, ChCode::Digit, unicode_version(flags))
    } else {
        c.is_ascii_digit()
    }
}

fn is_space(c: char, flags: u32) -> bool {
    if unicode(flags) {
        categories::is_space_by_code(c)
            || (!c.is_ascii() && categories::in_class(c, ChCode::Space, unicode_version(flags)))
    } else {
        matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
    }
}

fn is_word(c: char, flags: u32) -> bool {
    if unicode(flags) && !c.is_ascii() {
        categories::in_class(c, ChCode::Word, unicode_version(flags))
    } else {
        c == '_' || c.is_ascii_alphanumeric()
    }
}

pub(super) fn at_matches(at: AtCode, text: &[char], pos: usize, flags: u32) -> bool {
    let multiline = flags & SRE_FLAG_MULTILINE != 0;
    match at {
        AtCode::BeginningString => pos == 0,
        AtCode::Beginning => pos == 0 || (multiline && text[pos - 1] == '\n'),
        AtCode::EndString => pos == text.len(),
        AtCode::End => {
            pos == text.len() || (text[pos] == '\n' && (multiline || pos + 1 == text.len()))
        }
        AtCode::Boundary => is_word_boundary(text, pos, flags),
        AtCode::NonBoundary => !is_word_boundary(text, pos, flags),
    }
}

fn is_word_boundary(text: &[char], pos: usize, flags: u32) -> bool {
    let before = pos > 0 && is_word(text[pos - 1], flags);
    let after = pos < text.len() && is_word(text[pos], flags);
    before != after
}

/// How many characters from `from` on match the unit of the unbounded
/// greedy repeat `run_id` (a [`Touch::Tail`]) in `parsed`; `None` when
/// `parsed` has no such repeat.
pub fn run_len(parsed: &ParsedPattern, run_id: usize, text: &[char], from: usize) -> Option<usize> {
    let (unit, flags) = find_run(&parsed.pattern, parsed.flags, run_id)?;
    Some(
        text[from.min(text.len())..]
            .iter()
            .take_while(|&&c| unit_matches(unit, c, flags))
            .count(),
    )
}

/// Whether `probe` (from a [`Touch::Unless`] of a match of `parsed`)
/// matches `c`; `None` when `parsed` has no such test.
pub fn probe_matches(parsed: &ParsedPattern, probe: &Probe, c: char) -> Option<bool> {
    let (unit, flags) = match *probe {
        Probe::Literal(cp, flags) => (UnitOp::Literal(cp), flags),
        Probe::NotLiteral(cp, flags) => (UnitOp::NotLiteral(cp), flags),
        Probe::Any(flags) => (UnitOp::Any, flags),
        Probe::In(items, flags) => (UnitOp::In(find_set(&parsed.pattern, items)?), flags),
        Probe::Run(run_id) => find_run(&parsed.pattern, parsed.flags, run_id)?,
    };
    Some(unit_matches(unit, c, flags))
}

/// The items of the character class at address `items`, searched for
/// through `p`.
fn find_set(p: &SubPattern, items: usize) -> Option<&[SetItem]> {
    for op in &p.data {
        let found = match op {
            OpCode::In(candidates) if candidates.as_ptr() as usize == items => {
                Some(candidates.as_slice())
            }
            OpCode::MaxRepeat { item, .. }
            | OpCode::MinRepeat { item, .. }
            | OpCode::PossessiveRepeat { item, .. } => find_set(item, items),
            OpCode::Subpattern { p, .. }
            | OpCode::AtomicGroup(p)
            | OpCode::Assert { p, .. }
            | OpCode::AssertNot { p, .. } => find_set(p, items),
            OpCode::Branch(alternatives) => alternatives
                .iter()
                .find_map(|alternative| find_set(alternative, items)),
            OpCode::GroupRefExists { yes, no, .. } => {
                find_set(yes, items).or_else(|| no.as_ref().and_then(|no| find_set(no, items)))
            }
            _ => None,
        };
        if found.is_some() {
            return found;
        }
    }
    None
}

/// The single unit, with its flags, of the unbounded greedy repeat whose
/// item is at address `run_id`, searched for through `p`.
fn find_run(p: &SubPattern, flags: u32, run_id: usize) -> Option<(UnitOp<'_>, u32)> {
    for op in &p.data {
        let found = match op {
            OpCode::MaxRepeat { item, max, .. }
                if *max == MAXREPEAT && core::ptr::from_ref(item) as usize == run_id =>
            {
                single_unit(item, flags)
            }
            OpCode::MaxRepeat { item, .. }
            | OpCode::MinRepeat { item, .. }
            | OpCode::PossessiveRepeat { item, .. } => find_run(item, flags, run_id),
            OpCode::Subpattern {
                add_flags,
                del_flags,
                p,
                ..
            } => find_run(p, combine_flags(flags, *add_flags, *del_flags), run_id),
            OpCode::AtomicGroup(p) => find_run(p, flags, run_id),
            OpCode::Branch(alternatives) => alternatives
                .iter()
                .find_map(|alternative| find_run(alternative, flags, run_id)),
            OpCode::GroupRefExists { yes, no, .. } => find_run(yes, flags, run_id)
                .or_else(|| no.as_ref().and_then(|no| find_run(no, flags, run_id))),
            _ => None,
        };
        if found.is_some() {
            return found;
        }
    }
    None
}

#[cfg(test)]
#[path = "../../../tests/embedded/native/re_matcher_tests.rs"]
mod tests;
