//! Pure `#![no_std]` linear-time Thompson NFA / PikeVM regular expression engine.
//!
//! Guaranteed `O(n * m)` worst-case matching time (where `n` is the input length and `m` is the
//! compiled NFA state count) — never catastrophic backtracking — with explicit compile-time
//! ([`RegexLimits`]) and match-time (`max_steps`) budgets and caller-named allocators.
//!
//! # Supported syntax
//!
//! - Literals, `.` (any char except `\n`, or any char under `(?s)`), and escapes (`\d`, `\D`,
//!   `\w`, `\W`, `\s`, `\S`, `\n`, `\r`, `\t`, `\f`, `\v`, `\0`, `\\`, `\xHH`, `\x{H...}`, and
//!   escaped ASCII punctuation).
//! - Zero-width anchors: `^`, `$`, `\A`, `\z`, `\b`, `\B` (`^` and `$` become line-aware under
//!   `(?m)`).
//! - Character classes `[a-z0-9_]`, negated classes `[^...]`, ranges, and shorthands inside
//!   classes.
//! - Alternation `a|b`, capturing groups `(...)`, non-capturing groups `(?:...)`, and inline
//!   flags `(?i)`, `(?s)`, `(?m)`, negated forms `(?-i)`, and scoped flag groups `(?i:...)`.
//! - Greedy and lazy quantifiers (`*`, `+`, `?`, `*?`, `+?`, `??`) and bounded counted
//!   repetitions (`{m}`, `{m,}`, `{m,n}`, and lazy `{m,n}?`).
//!
//! # Construction & execution
//!
//! Both the parser and the Thompson NFA compiler / PikeVM epsilon-closure are 100% iterative
//! (zero recursion), zero-`unsafe`, and panic-free.

use core::alloc::Allocator;
use core::fmt;

use blaze_own::{AllocFailure, String, Vec};

mod nfa;
mod parse;
#[cfg(test)]
mod tests;

use nfa::{Nfa, PikeVmScratch};

/// Explicit compile-time and default execution limits for [`Regex`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RegexLimits {
    /// Maximum pattern length in bytes.
    pub most_pattern_bytes: usize,
    /// Maximum number of Thompson NFA states produced during compilation.
    pub most_states: usize,
    /// Maximum number of explicit capturing groups `(...)` (group 0 is always present).
    pub most_captures: usize,
    /// Maximum repetition bound in `{m}`, `{m,}`, or `{m,n}`.
    pub most_repeat: usize,
    /// Default step budget for matching operations.
    pub most_steps: usize,
}

impl RegexLimits {
    /// Generous default limits suitable for general text matching (64 KiB pattern, 16,384 states,
    /// 64 capturing groups, 1,000 max repeat count, 10,000,000 steps).
    pub const USUAL: Self = Self {
        most_pattern_bytes: 64 * 1024,
        most_states: 16_384,
        most_captures: 64,
        most_repeat: 1_000,
        most_steps: 10_000_000,
    };

    /// Alias matching `blaze_json::Limits::SHIPPED` and `bstd::diff::ParsePatchLimits::SHIPPED`.
    pub const SHIPPED: Self = Self::USUAL;
}

/// Why compiling or executing a [`Regex`] failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RegexError {
    /// Byte offset in the pattern (for compile errors) or text (for match errors).
    pub offset: usize,
    /// Specific syntactic, budget, or allocation failure.
    pub kind: RegexErrorKind,
}

/// Specific reason a [`Regex`] compile or match call failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RegexErrorKind {
    /// Pattern byte length exceeded `RegexLimits::most_pattern_bytes`.
    PatternTooLarge { bytes: usize, limit: usize },
    /// Opening `(` had no matching `)`.
    UnclosedGroup,
    /// Encountered `)` without an open group.
    UnexpectedCloseParen,
    /// Opening `[` had no matching `]`.
    UnclosedClass,
    /// Character class range `start-end` had `start > end` or used a shorthand as an endpoint.
    InvalidClassRange { start: char, end: char },
    /// Unrecognized, truncated, or invalid escape sequence.
    InvalidEscape,
    /// Quantifier had nothing repeatable before it or malformed `{m,n}` syntax.
    InvalidQuantifier,
    /// Unrecognized or malformed inline flag group `(?...)`.
    InvalidFlag,
    /// Compiled NFA exceeded `RegexLimits::most_states`.
    TooManyStates { limit: usize },
    /// Explicit capturing group count exceeded `RegexLimits::most_captures`.
    TooManyCaptures { limit: usize },
    /// Counted repetition `{m,n}` exceeded `RegexLimits::most_repeat`.
    RepeatTooLarge { count: usize, limit: usize },
    /// PikeVM execution exceeded the caller's `max_steps` budget.
    StepLimitExceeded { steps: usize, limit: usize },
    /// The caller's allocator refused an allocation.
    Alloc(AllocFailure),
}

impl RegexError {
    #[inline]
    pub(crate) const fn new(offset: usize, kind: RegexErrorKind) -> Self {
        Self { offset, kind }
    }

    #[inline]
    pub(crate) const fn alloc(err: AllocFailure) -> Self {
        Self { offset: 0, kind: RegexErrorKind::Alloc(err) }
    }
}

impl fmt::Display for RegexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            RegexErrorKind::PatternTooLarge { bytes, limit } => {
                write!(f, "bstd::regex: pattern size {bytes} bytes exceeds limit {limit}")
            }
            RegexErrorKind::UnclosedGroup => {
                write!(f, "bstd::regex: byte {}: unclosed group `(`", self.offset)
            }
            RegexErrorKind::UnexpectedCloseParen => {
                write!(f, "bstd::regex: byte {}: unexpected closing `)`", self.offset)
            }
            RegexErrorKind::UnclosedClass => {
                write!(f, "bstd::regex: byte {}: unclosed character class `[`", self.offset)
            }
            RegexErrorKind::InvalidClassRange { start, end } => {
                write!(
                    f,
                    "bstd::regex: byte {}: invalid character class range `{start}-{end}`",
                    self.offset
                )
            }
            RegexErrorKind::InvalidEscape => {
                write!(f, "bstd::regex: byte {}: invalid escape sequence", self.offset)
            }
            RegexErrorKind::InvalidQuantifier => {
                write!(f, "bstd::regex: byte {}: invalid quantifier", self.offset)
            }
            RegexErrorKind::InvalidFlag => {
                write!(f, "bstd::regex: byte {}: invalid inline flag", self.offset)
            }
            RegexErrorKind::TooManyStates { limit } => {
                write!(
                    f,
                    "bstd::regex: byte {}: compiled NFA exceeded state limit ({limit})",
                    self.offset
                )
            }
            RegexErrorKind::TooManyCaptures { limit } => {
                write!(
                    f,
                    "bstd::regex: byte {}: capturing group count exceeded limit ({limit})",
                    self.offset
                )
            }
            RegexErrorKind::RepeatTooLarge { count, limit } => {
                write!(
                    f,
                    "bstd::regex: byte {}: repetition count {count} exceeds limit {limit}",
                    self.offset
                )
            }
            RegexErrorKind::StepLimitExceeded { steps, limit } => {
                write!(
                    f,
                    "bstd::regex: byte {}: match exceeded step budget ({steps} / {limit})",
                    self.offset
                )
            }
            RegexErrorKind::Alloc(err) => {
                write!(f, "bstd::regex: allocation failed ({err})")
            }
        }
    }
}

impl core::error::Error for RegexError {}

/// Byte span of a match in the searched text (`start..end`, guaranteed UTF-8 char boundaries).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Match {
    /// Inclusive start byte offset.
    pub start: usize,
    /// Exclusive end byte offset.
    pub end: usize,
}

impl Match {
    /// Borrow the matched slice from `text`.
    #[inline]
    pub fn as_str<'t>(&self, text: &'t str) -> &'t str {
        text.get(self.start..self.end).unwrap_or("")
    }

    /// Length of the match in bytes.
    #[inline]
    pub const fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    /// Whether this match is zero-length.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

/// Captured group spans from a single regex match (group `0` is always the full match).
#[derive(Debug)]
pub struct Captures<A: Allocator> {
    groups: Vec<Option<Match>, A>,
}

impl<A: Allocator> Captures<A> {
    /// Return the matched byte span for capturing group `group` (`0` = full match).
    #[inline]
    pub fn get(&self, group: usize) -> Option<Match> {
        self.groups.get(group).copied().flatten()
    }

    /// Return the matched slice of `text` for capturing group `group` (`0` = full match).
    #[inline]
    pub fn group_str<'t>(&self, text: &'t str, group: usize) -> Option<&'t str> {
        let m = self.get(group)?;
        text.get(m.start..m.end)
    }

    /// Total number of capturing groups (including group `0`).
    #[inline]
    pub fn len(&self) -> usize {
        self.groups.len()
    }

    /// Whether there are zero capture slots.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }
}

/// Test-only planted bugs for verifying property-test oracles (`blaze_sim::control`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RegexPlant {
    None,
    /// Swap greedy and lazy split branch priority during NFA compilation.
    InvertGreediness,
    /// Ignore `(?i)` case-insensitivity during parsing.
    IgnoreCaseFlag,
}

/// A compiled linear-time Thompson NFA regular expression.
#[derive(Debug)]
pub struct Regex<A: Allocator> {
    nfa: Nfa<A>,
}

impl<A: Allocator> Regex<A> {
    /// Compile `pattern` into a Thompson NFA under explicit `limits` in allocator `alloc`.
    pub fn compile(pattern: &str, limits: RegexLimits, alloc: A) -> Result<Self, RegexError>
    where
        A: Copy,
    {
        Self::compile_planted(pattern, limits, alloc, RegexPlant::None)
    }

    pub(crate) fn compile_planted(
        pattern: &str,
        limits: RegexLimits,
        alloc: A,
        plant: RegexPlant,
    ) -> Result<Self, RegexError>
    where
        A: Copy,
    {
        let ast = parse::parse_pattern(pattern, limits, alloc, plant)?;
        let nfa = nfa::compile_ast(ast, limits, alloc, plant)?;
        Ok(Self { nfa })
    }

    /// Number of states in the compiled NFA.
    #[inline]
    pub fn state_count(&self) -> usize {
        self.nfa.state_count()
    }

    /// Total number of capturing groups (including group `0`).
    #[inline]
    pub fn capture_count(&self) -> usize {
        self.nfa.num_groups()
    }

    /// Return `true` if `self` matches anywhere in `text` within `max_steps` PikeVM steps.
    pub fn is_match<S: Allocator + Copy>(
        &self,
        text: &str,
        max_steps: usize,
        scratch: S,
    ) -> Result<bool, RegexError> {
        let mut vm = PikeVmScratch::new(self.nfa.state_count(), 0, scratch)?;
        let mut remaining = max_steps;
        let matched = self.nfa.search_from(text, 0, &mut vm, &mut remaining, max_steps)?;
        Ok(matched)
    }

    /// Find the leftmost-first match in `text` within `max_steps` PikeVM steps.
    pub fn find<S: Allocator + Copy>(
        &self,
        text: &str,
        max_steps: usize,
        scratch: S,
    ) -> Result<Option<Match>, RegexError> {
        let mut vm = PikeVmScratch::new(self.nfa.state_count(), 2, scratch)?;
        let mut remaining = max_steps;
        if !self.nfa.search_from(text, 0, &mut vm, &mut remaining, max_steps)? {
            return Ok(None);
        }
        Ok(vm.match_zero())
    }

    /// Find all non-overlapping leftmost-first matches in `text`, sharing `max_steps` across the
    /// entire scan.
    pub fn find_all<S: Allocator + Copy>(
        &self,
        text: &str,
        max_steps: usize,
        alloc: S,
    ) -> Result<Vec<Match, S>, RegexError> {
        let mut vm = PikeVmScratch::new(self.nfa.state_count(), 2, alloc)?;
        let mut out = Vec::new_in(alloc);
        let mut remaining = max_steps;
        let mut search_pos = 0usize;

        while search_pos <= text.len() {
            if !self.nfa.search_from(text, search_pos, &mut vm, &mut remaining, max_steps)? {
                break;
            }
            let Some(m) = vm.match_zero() else {
                break;
            };
            out.try_push(m).map_err(|r| RegexError::alloc(r.why))?;
            if m.end > m.start {
                search_pos = m.end;
            } else if let Some(next_pos) = next_char_boundary(text, m.end) {
                search_pos = next_pos;
            } else {
                break;
            }
        }
        Ok(out)
    }

    /// Find the leftmost-first match and all capturing group spans in `text`.
    pub fn captures<S: Allocator + Copy>(
        &self,
        text: &str,
        max_steps: usize,
        alloc: S,
    ) -> Result<Option<Captures<S>>, RegexError> {
        let slots = self.nfa.num_slots();
        let mut vm = PikeVmScratch::new(self.nfa.state_count(), slots, alloc)?;
        let mut remaining = max_steps;
        if !self.nfa.search_from(text, 0, &mut vm, &mut remaining, max_steps)? {
            return Ok(None);
        }
        let groups = vm.extract_groups(self.nfa.num_groups(), alloc)?;
        Ok(Some(Captures { groups }))
    }

    /// Replace every non-overlapping match in `text` with `replacement`.
    ///
    /// Supports capture-group expansion in `replacement`: `$0`, `$1`, `${1}`, and `$$` (literal
    /// `$`). An unescaped `$` not followed by digits, `{digits}`, or `$` is emitted literally.
    pub fn replace_all<S: Allocator + Copy>(
        &self,
        text: &str,
        replacement: &str,
        max_steps: usize,
        alloc: S,
    ) -> Result<String<S>, RegexError> {
        let uses_captures = replacement.as_bytes().contains(&b'$');
        let slots = if uses_captures { self.nfa.num_slots() } else { 2 };
        let mut vm = PikeVmScratch::new(self.nfa.state_count(), slots, alloc)?;
        let mut out = String::try_with_capacity_in(text.len(), alloc).map_err(RegexError::alloc)?;
        let mut remaining = max_steps;
        let mut search_pos = 0usize;
        let mut last_copied = 0usize;

        while search_pos <= text.len() {
            if !self.nfa.search_from(text, search_pos, &mut vm, &mut remaining, max_steps)? {
                break;
            }
            let Some(m) = vm.match_zero() else {
                break;
            };
            if let Some(prefix) = text.get(last_copied..m.start) {
                out.try_push_str(prefix).map_err(RegexError::alloc)?;
            }
            if uses_captures {
                expand_replacement(
                    text,
                    replacement,
                    vm.best_slots(),
                    self.nfa.num_groups(),
                    &mut out,
                )?;
            } else {
                out.try_push_str(replacement).map_err(RegexError::alloc)?;
            }
            last_copied = m.end;
            if m.end > m.start {
                search_pos = m.end;
            } else if let Some(next_pos) = next_char_boundary(text, m.end) {
                search_pos = next_pos;
            } else {
                break;
            }
        }
        if let Some(tail) = text.get(last_copied..) {
            out.try_push_str(tail).map_err(RegexError::alloc)?;
        }
        Ok(out)
    }

    /// Split `text` on every non-overlapping match, returning borrowed slices of `text`.
    pub fn split<'t, S: Allocator + Copy>(
        &self,
        text: &'t str,
        max_steps: usize,
        alloc: S,
    ) -> Result<Vec<&'t str, S>, RegexError> {
        let mut vm = PikeVmScratch::new(self.nfa.state_count(), 2, alloc)?;
        let mut out = Vec::new_in(alloc);
        let mut remaining = max_steps;
        let mut search_pos = 0usize;
        let mut last_end = 0usize;

        while search_pos <= text.len() {
            if !self.nfa.search_from(text, search_pos, &mut vm, &mut remaining, max_steps)? {
                break;
            }
            let Some(m) = vm.match_zero() else {
                break;
            };
            let piece = text.get(last_end..m.start).unwrap_or("");
            out.try_push(piece).map_err(|r| RegexError::alloc(r.why))?;
            last_end = m.end;
            if m.end > m.start {
                search_pos = m.end;
            } else if let Some(next_pos) = next_char_boundary(text, m.end) {
                search_pos = next_pos;
            } else {
                break;
            }
        }
        let tail = text.get(last_end..).unwrap_or("");
        out.try_push(tail).map_err(|r| RegexError::alloc(r.why))?;
        Ok(out)
    }
}

/// Return the next UTF-8 character boundary strictly after `pos`, or `None` at end of `text`.
#[inline]
fn next_char_boundary(text: &str, pos: usize) -> Option<usize> {
    let rest = text.get(pos..)?;
    let ch = rest.chars().next()?;
    pos.checked_add(ch.len_utf8())
}

/// Expand `$0`, `$N`, `${N}`, and `$$` in `replacement` using captured `slots`.
fn expand_replacement<A: Allocator>(
    text: &str,
    replacement: &str,
    slots: &[Option<usize>],
    num_groups: usize,
    out: &mut String<A>,
) -> Result<(), RegexError> {
    let bytes = replacement.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let Some(&b) = bytes.get(i) else {
            break;
        };
        if b != b'$' {
            let start = i;
            i = i.saturating_add(1);
            while let Some(&next_b) = bytes.get(i) {
                if next_b == b'$' {
                    break;
                }
                i = i.saturating_add(1);
            }
            if let Some(lit) = replacement.get(start..i) {
                out.try_push_str(lit).map_err(RegexError::alloc)?;
            }
            continue;
        }
        // Found `$` at `i`.
        match bytes.get(i.saturating_add(1)).copied() {
            Some(b'$') => {
                out.try_push('$').map_err(RegexError::alloc)?;
                i = i.saturating_add(2);
            }
            Some(b'{') => {
                let mut j = i.saturating_add(2);
                let digit_start = j;
                let mut group = 0usize;
                let mut overflow = false;
                while let Some(&d) = bytes.get(j) {
                    if !d.is_ascii_digit() {
                        break;
                    }
                    let digit = usize::from(d.saturating_sub(b'0'));
                    match group.checked_mul(10).and_then(|g| g.checked_add(digit)) {
                        Some(v) => group = v,
                        None => overflow = true,
                    }
                    j = j.saturating_add(1);
                }
                if j > digit_start && bytes.get(j).copied() == Some(b'}') {
                    if !overflow {
                        push_group_span(text, slots, num_groups, group, out)?;
                    }
                    i = j.saturating_add(1);
                } else {
                    out.try_push('$').map_err(RegexError::alloc)?;
                    i = i.saturating_add(1);
                }
            }
            Some(d) if d.is_ascii_digit() => {
                let mut j = i.saturating_add(1);
                let mut group = 0usize;
                let mut overflow = false;
                while let Some(&ch_b) = bytes.get(j) {
                    if !ch_b.is_ascii_digit() {
                        break;
                    }
                    let digit = usize::from(ch_b.saturating_sub(b'0'));
                    match group.checked_mul(10).and_then(|g| g.checked_add(digit)) {
                        Some(v) => group = v,
                        None => overflow = true,
                    }
                    j = j.saturating_add(1);
                }
                if !overflow {
                    push_group_span(text, slots, num_groups, group, out)?;
                }
                i = j;
            }
            Some(_) | None => {
                out.try_push('$').map_err(RegexError::alloc)?;
                i = i.saturating_add(1);
            }
        }
    }
    Ok(())
}

fn push_group_span<A: Allocator>(
    text: &str,
    slots: &[Option<usize>],
    num_groups: usize,
    group: usize,
    out: &mut String<A>,
) -> Result<(), RegexError> {
    if group >= num_groups {
        return Ok(());
    }
    let Some(start_slot) = group.checked_mul(2) else {
        return Ok(());
    };
    let Some(end_slot) = start_slot.checked_add(1) else {
        return Ok(());
    };
    if let (Some(Some(s)), Some(Some(e))) = (slots.get(start_slot), slots.get(end_slot))
        && let Some(slice) = text.get(*s..*e)
    {
        out.try_push_str(slice).map_err(RegexError::alloc)?;
    }
    Ok(())
}
