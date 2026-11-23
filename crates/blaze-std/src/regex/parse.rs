//! Pure iterative regex parser producing a flat arena-allocated AST.
//!
//! Zero recursion, zero `unsafe`, zero panics, zero integer `as` casts.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{RegexError, RegexErrorKind, RegexLimits, RegexPlant};

/// Active inline regex flags.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Flags {
    case_insensitive: bool,
    dot_all: bool,
    multi_line: bool,
}

impl Flags {
    const DEFAULT: Self = Self { case_insensitive: false, dot_all: false, multi_line: false };
}

/// Dense index into [`Ast::nodes`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct NodeId(pub(crate) u32);

/// Inclusive Unicode scalar range inside a character class.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct CharRange {
    pub(crate) start: char,
    pub(crate) end: char,
}

/// Zero-width position assertion.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum AnchorKind {
    StartText,
    EndText,
    StartLine,
    EndLine,
    WordBoundary,
    NonWordBoundary,
}

/// A flat, `Copy` AST node stored inside [`Ast::nodes`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Node {
    Empty,
    Literal {
        ch: char,
        case_insensitive: bool,
    },
    AnyChar {
        dot_all: bool,
    },
    Class {
        range_start: u32,
        range_len: u32,
        negated: bool,
    },
    Anchor(AnchorKind),
    Concat {
        children_start: u32,
        children_len: u32,
    },
    Alt {
        children_start: u32,
        children_len: u32,
    },
    Capture {
        group: u32,
        inner: NodeId,
    },
    Repeat {
        inner: NodeId,
        min: u32,
        max: Option<u32>,
        greedy: bool,
    },
}

/// Flat arena-allocated regex AST.
#[derive(Debug)]
pub(crate) struct Ast<A: Allocator> {
    pub(crate) nodes: Vec<Node, A>,
    pub(crate) children: Vec<NodeId, A>,
    pub(crate) ranges: Vec<CharRange, A>,
    pub(crate) root: NodeId,
    /// Total number of capturing groups including group `0`.
    pub(crate) num_groups: usize,
}

#[derive(Clone, Copy)]
enum Shorthand {
    Digit,
    NotDigit,
    Word,
    NotWord,
    Space,
    NotSpace,
}

#[derive(Clone, Copy)]
enum ClassAtom {
    Char(char),
    Shorthand(Shorthand),
}

#[derive(Clone, Copy)]
struct GroupFrame {
    open_offset: usize,
    capture_group: Option<u32>,
    saved_flags: Flags,
    seq_start: usize,
    alt_start: usize,
    can_quantify: bool,
}

/// Cursor over a UTF-8 pattern string with zero unchecked indexing.
struct Cursor<'a> {
    pattern: &'a str,
    pos: usize,
}

impl<'a> Cursor<'a> {
    #[inline]
    const fn new(pattern: &'a str) -> Self {
        Self { pattern, pos: 0 }
    }

    #[inline]
    fn rest(&self) -> &'a str {
        self.pattern.get(self.pos..).unwrap_or("")
    }

    #[inline]
    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    #[inline]
    fn peek_second(&self) -> Option<char> {
        let mut iter = self.rest().chars();
        let _ = iter.next()?;
        iter.next()
    }

    #[inline]
    fn next_char(&mut self) -> Option<(usize, char)> {
        let offset = self.pos;
        let ch = self.rest().chars().next()?;
        self.pos = self.pos.saturating_add(ch.len_utf8());
        Some((offset, ch))
    }

    #[inline]
    fn eat(&mut self, expected: char) -> bool {
        if self.peek() == Some(expected) {
            self.pos = self.pos.saturating_add(expected.len_utf8());
            true
        } else {
            false
        }
    }
}

/// Parse `pattern` into a flat [`Ast`] using explicit stacks (zero recursion).
pub(crate) fn parse_pattern<A: Allocator + Copy>(
    pattern: &str,
    limits: RegexLimits,
    alloc: A,
    plant: RegexPlant,
) -> Result<Ast<A>, RegexError> {
    if pattern.len() > limits.most_pattern_bytes {
        return Err(RegexError::new(
            0,
            RegexErrorKind::PatternTooLarge {
                bytes: pattern.len(),
                limit: limits.most_pattern_bytes,
            },
        ));
    }

    let mut nodes: Vec<Node, A> = Vec::new_in(alloc);
    let mut children: Vec<NodeId, A> = Vec::new_in(alloc);
    let mut ranges: Vec<CharRange, A> = Vec::new_in(alloc);

    let mut frames: Vec<GroupFrame, A> = Vec::new_in(alloc);
    let mut seq_stack: Vec<NodeId, A> = Vec::new_in(alloc);
    let mut alt_stack: Vec<NodeId, A> = Vec::new_in(alloc);

    frames
        .try_push(GroupFrame {
            open_offset: 0,
            capture_group: None,
            saved_flags: Flags::DEFAULT,
            seq_start: 0,
            alt_start: 0,
            can_quantify: false,
        })
        .map_err(|r| RegexError::alloc(r.why))?;

    let mut flags = Flags::DEFAULT;
    let mut explicit_captures = 0usize;
    let mut cursor = Cursor::new(pattern);

    while let Some((offset, ch)) = cursor.next_char() {
        match ch {
            '(' => {
                let open_flags = flags;
                if cursor.eat('?') {
                    if cursor.eat(':') {
                        frames
                            .try_push(GroupFrame {
                                open_offset: offset,
                                capture_group: None,
                                saved_flags: open_flags,
                                seq_start: seq_stack.len(),
                                alt_start: alt_stack.len(),
                                can_quantify: false,
                            })
                            .map_err(|r| RegexError::alloc(r.why))?;
                    } else {
                        let (new_flags, scoped) = parse_inline_flags(&mut cursor, offset, flags, plant)?;
                        flags = new_flags;
                        if scoped {
                            frames
                                .try_push(GroupFrame {
                                    open_offset: offset,
                                    capture_group: None,
                                    saved_flags: open_flags,
                                    seq_start: seq_stack.len(),
                                    alt_start: alt_stack.len(),
                                    can_quantify: false,
                                })
                                .map_err(|r| RegexError::alloc(r.why))?;
                        } else if let Some(top) = frames.last_mut() {
                            top.can_quantify = false;
                        }
                    }
                } else {
                    explicit_captures = explicit_captures.saturating_add(1);
                    if explicit_captures > limits.most_captures {
                        return Err(RegexError::new(
                            offset,
                            RegexErrorKind::TooManyCaptures { limit: limits.most_captures },
                        ));
                    }
                    let group_u32 = u32::try_from(explicit_captures).map_err(|_| {
                        RegexError::new(
                            offset,
                            RegexErrorKind::TooManyCaptures { limit: limits.most_captures },
                        )
                    })?;
                    frames
                        .try_push(GroupFrame {
                            open_offset: offset,
                            capture_group: Some(group_u32),
                            saved_flags: open_flags,
                            seq_start: seq_stack.len(),
                            alt_start: alt_stack.len(),
                            can_quantify: false,
                        })
                        .map_err(|r| RegexError::alloc(r.why))?;
                }
            }
            ')' => {
                if frames.len() <= 1 {
                    return Err(RegexError::new(offset, RegexErrorKind::UnexpectedCloseParen));
                }
                let Some(frame) = frames.pop() else {
                    return Err(RegexError::new(offset, RegexErrorKind::UnexpectedCloseParen));
                };
                let branch = finish_sequence(&mut nodes, &mut children, &mut seq_stack, frame.seq_start, offset)?;
                alt_stack.try_push(branch).map_err(|r| RegexError::alloc(r.why))?;
                let inner = finish_alternation(&mut nodes, &mut children, &mut alt_stack, frame.alt_start, offset)?;
                let group_node = match frame.capture_group {
                    Some(group) => push_node(&mut nodes, Node::Capture { group, inner }, offset)?,
                    None => inner,
                };
                flags = frame.saved_flags;
                seq_stack.try_push(group_node).map_err(|r| RegexError::alloc(r.why))?;
                if let Some(parent) = frames.last_mut() {
                    parent.can_quantify = true;
                }
            }
            '|' => {
                let Some(frame) = frames.last_mut() else {
                    return Err(RegexError::new(offset, RegexErrorKind::UnexpectedCloseParen));
                };
                let branch = finish_sequence(&mut nodes, &mut children, &mut seq_stack, frame.seq_start, offset)?;
                alt_stack.try_push(branch).map_err(|r| RegexError::alloc(r.why))?;
                frame.can_quantify = false;
            }
            '.' => {
                let id = push_node(&mut nodes, Node::AnyChar { dot_all: flags.dot_all }, offset)?;
                push_atom(&mut frames, &mut seq_stack, id)?;
            }
            '^' => {
                let kind = if flags.multi_line {
                    AnchorKind::StartLine
                } else {
                    AnchorKind::StartText
                };
                let id = push_node(&mut nodes, Node::Anchor(kind), offset)?;
                push_atom(&mut frames, &mut seq_stack, id)?;
            }
            '$' => {
                let kind = if flags.multi_line {
                    AnchorKind::EndLine
                } else {
                    AnchorKind::EndText
                };
                let id = push_node(&mut nodes, Node::Anchor(kind), offset)?;
                push_atom(&mut frames, &mut seq_stack, id)?;
            }
            '[' => {
                let id = parse_char_class(
                    &mut cursor,
                    offset,
                    flags,
                    &mut nodes,
                    &mut ranges,
                    plant,
                )?;
                push_atom(&mut frames, &mut seq_stack, id)?;
            }
            '\\' => {
                let id = parse_escape_outside(
                    &mut cursor,
                    offset,
                    flags,
                    &mut nodes,
                    &mut ranges,
                    plant,
                )?;
                push_atom(&mut frames, &mut seq_stack, id)?;
            }
            '*' | '+' | '?' => {
                let greedy = !cursor.eat('?');
                let (min, max) = match ch {
                    '*' => (0u32, None),
                    '+' => (1u32, None),
                    _ => (0u32, Some(1u32)),
                };
                apply_quantifier(&mut frames, &mut seq_stack, &mut nodes, offset, min, max, greedy)?;
            }
            '{' => {
                let (min, max, greedy) = parse_counted_repeat(&mut cursor, offset, limits)?;
                apply_quantifier(&mut frames, &mut seq_stack, &mut nodes, offset, min, max, greedy)?;
            }
            other => {
                let ci = flags.case_insensitive && plant != RegexPlant::IgnoreCaseFlag;
                let id = push_node(
                    &mut nodes,
                    Node::Literal { ch: other, case_insensitive: ci },
                    offset,
                )?;
                push_atom(&mut frames, &mut seq_stack, id)?;
            }
        }
    }

    if frames.len() > 1 {
        let unclosed_offset = frames.last().map_or(0, |f| f.open_offset);
        return Err(RegexError::new(unclosed_offset, RegexErrorKind::UnclosedGroup));
    }

    let root_frame = frames.pop().unwrap_or(GroupFrame {
        open_offset: 0,
        capture_group: None,
        saved_flags: Flags::DEFAULT,
        seq_start: 0,
        alt_start: 0,
        can_quantify: false,
    });
    let final_branch = finish_sequence(&mut nodes, &mut children, &mut seq_stack, root_frame.seq_start, pattern.len())?;
    alt_stack.try_push(final_branch).map_err(|r| RegexError::alloc(r.why))?;
    let root = finish_alternation(&mut nodes, &mut children, &mut alt_stack, root_frame.alt_start, pattern.len())?;

    Ok(Ast {
        nodes,
        children,
        ranges,
        root,
        num_groups: explicit_captures.saturating_add(1),
    })
}

fn push_node<A: Allocator>(
    nodes: &mut Vec<Node, A>,
    node: Node,
    offset: usize,
) -> Result<NodeId, RegexError> {
    let id = u32::try_from(nodes.len()).map_err(|_| {
        RegexError::new(offset, RegexErrorKind::TooManyStates { limit: usize::try_from(u32::MAX).unwrap_or(usize::MAX) })
    })?;
    nodes.try_push(node).map_err(|r| RegexError::alloc(r.why))?;
    Ok(NodeId(id))
}

fn push_atom<A: Allocator>(
    frames: &mut Vec<GroupFrame, A>,
    seq_stack: &mut Vec<NodeId, A>,
    id: NodeId,
) -> Result<(), RegexError> {
    seq_stack.try_push(id).map_err(|r| RegexError::alloc(r.why))?;
    if let Some(frame) = frames.last_mut() {
        frame.can_quantify = true;
    }
    Ok(())
}

fn apply_quantifier<A: Allocator>(
    frames: &mut Vec<GroupFrame, A>,
    seq_stack: &mut Vec<NodeId, A>,
    nodes: &mut Vec<Node, A>,
    offset: usize,
    min: u32,
    max: Option<u32>,
    greedy: bool,
) -> Result<(), RegexError> {
    let Some(frame) = frames.last_mut() else {
        return Err(RegexError::new(offset, RegexErrorKind::InvalidQuantifier));
    };
    if !frame.can_quantify || seq_stack.len() <= frame.seq_start {
        return Err(RegexError::new(offset, RegexErrorKind::InvalidQuantifier));
    }
    let Some(inner) = seq_stack.pop() else {
        return Err(RegexError::new(offset, RegexErrorKind::InvalidQuantifier));
    };
    let rep = push_node(nodes, Node::Repeat { inner, min, max, greedy }, offset)?;
    seq_stack.try_push(rep).map_err(|r| RegexError::alloc(r.why))?;
    frame.can_quantify = false;
    Ok(())
}

fn finish_sequence<A: Allocator>(
    nodes: &mut Vec<Node, A>,
    children: &mut Vec<NodeId, A>,
    seq_stack: &mut Vec<NodeId, A>,
    seq_start: usize,
    offset: usize,
) -> Result<NodeId, RegexError> {
    let count = seq_stack.len().saturating_sub(seq_start);
    if count == 0 {
        return push_node(nodes, Node::Empty, offset);
    }
    if count == 1 {
        let Some(single) = seq_stack.pop() else {
            return push_node(nodes, Node::Empty, offset);
        };
        return Ok(single);
    }
    let children_start = u32::try_from(children.len()).map_err(|_| {
        RegexError::new(offset, RegexErrorKind::TooManyStates { limit: usize::try_from(u32::MAX).unwrap_or(usize::MAX) })
    })?;
    let children_len = u32::try_from(count).map_err(|_| {
        RegexError::new(offset, RegexErrorKind::TooManyStates { limit: usize::try_from(u32::MAX).unwrap_or(usize::MAX) })
    })?;
    if let Some(slice) = seq_stack.get(seq_start..) {
        children.try_extend_from_slice(slice).map_err(RegexError::alloc)?;
    }
    seq_stack.truncate(seq_start);
    push_node(nodes, Node::Concat { children_start, children_len }, offset)
}

fn finish_alternation<A: Allocator>(
    nodes: &mut Vec<Node, A>,
    children: &mut Vec<NodeId, A>,
    alt_stack: &mut Vec<NodeId, A>,
    alt_start: usize,
    offset: usize,
) -> Result<NodeId, RegexError> {
    let count = alt_stack.len().saturating_sub(alt_start);
    if count == 0 {
        return push_node(nodes, Node::Empty, offset);
    }
    if count == 1 {
        let Some(single) = alt_stack.pop() else {
            return push_node(nodes, Node::Empty, offset);
        };
        return Ok(single);
    }
    let children_start = u32::try_from(children.len()).map_err(|_| {
        RegexError::new(offset, RegexErrorKind::TooManyStates { limit: usize::try_from(u32::MAX).unwrap_or(usize::MAX) })
    })?;
    let children_len = u32::try_from(count).map_err(|_| {
        RegexError::new(offset, RegexErrorKind::TooManyStates { limit: usize::try_from(u32::MAX).unwrap_or(usize::MAX) })
    })?;
    if let Some(slice) = alt_stack.get(alt_start..) {
        children.try_extend_from_slice(slice).map_err(RegexError::alloc)?;
    }
    alt_stack.truncate(alt_start);
    push_node(nodes, Node::Alt { children_start, children_len }, offset)
}

/// Parse inline flags after `(?`: returns `(updated_flags, is_scoped_group)`.
fn parse_inline_flags(
    cursor: &mut Cursor<'_>,
    open_offset: usize,
    mut flags: Flags,
    plant: RegexPlant,
) -> Result<(Flags, bool), RegexError> {
    let mut enabling = true;
    let mut seen_any_flag = false;
    let mut seen_minus = false;

    while let Some((off, ch)) = cursor.next_char() {
        match ch {
            'i' => {
                if plant != RegexPlant::IgnoreCaseFlag {
                    flags.case_insensitive = enabling;
                }
                seen_any_flag = true;
            }
            's' => {
                flags.dot_all = enabling;
                seen_any_flag = true;
            }
            'm' => {
                flags.multi_line = enabling;
                seen_any_flag = true;
            }
            '-' => {
                if seen_minus {
                    return Err(RegexError::new(off, RegexErrorKind::InvalidFlag));
                }
                seen_minus = true;
                enabling = false;
            }
            ')' => {
                if !seen_any_flag {
                    return Err(RegexError::new(open_offset, RegexErrorKind::InvalidFlag));
                }
                return Ok((flags, false));
            }
            ':' => {
                if !seen_any_flag {
                    return Err(RegexError::new(open_offset, RegexErrorKind::InvalidFlag));
                }
                return Ok((flags, true));
            }
            _ => {
                return Err(RegexError::new(off, RegexErrorKind::InvalidFlag));
            }
        }
    }
    Err(RegexError::new(open_offset, RegexErrorKind::InvalidFlag))
}

/// Parse `{m}`, `{m,}`, or `{m,n}` (with optional trailing `?` for lazy) after `{`.
fn parse_counted_repeat(
    cursor: &mut Cursor<'_>,
    open_offset: usize,
    limits: RegexLimits,
) -> Result<(u32, Option<u32>, bool), RegexError> {
    let min_usize = parse_decimal_digits(cursor, open_offset, limits.most_repeat)?;
    let max_opt = if cursor.eat(',') {
        if cursor.peek() == Some('}') {
            None
        } else {
            let max_usize = parse_decimal_digits(cursor, open_offset, limits.most_repeat)?;
            if max_usize < min_usize {
                return Err(RegexError::new(open_offset, RegexErrorKind::InvalidQuantifier));
            }
            let max_u32 = u32::try_from(max_usize).map_err(|_| {
                RegexError::new(
                    open_offset,
                    RegexErrorKind::RepeatTooLarge { count: max_usize, limit: limits.most_repeat },
                )
            })?;
            Some(max_u32)
        }
    } else {
        let min_u32 = u32::try_from(min_usize).map_err(|_| {
            RegexError::new(
                open_offset,
                RegexErrorKind::RepeatTooLarge { count: min_usize, limit: limits.most_repeat },
            )
        })?;
        Some(min_u32)
    };

    if !cursor.eat('}') {
        return Err(RegexError::new(open_offset, RegexErrorKind::InvalidQuantifier));
    }
    let min_u32 = u32::try_from(min_usize).map_err(|_| {
        RegexError::new(
            open_offset,
            RegexErrorKind::RepeatTooLarge { count: min_usize, limit: limits.most_repeat },
        )
    })?;
    let greedy = !cursor.eat('?');
    Ok((min_u32, max_opt, greedy))
}

fn parse_decimal_digits(
    cursor: &mut Cursor<'_>,
    offset: usize,
    limit: usize,
) -> Result<usize, RegexError> {
    let mut value = 0usize;
    let mut count = 0usize;
    while let Some(ch) = cursor.peek() {
        if !ch.is_ascii_digit() {
            break;
        }
        let _ = cursor.next_char();
        let digit = usize::try_from(u32::from(ch).saturating_sub(u32::from('0'))).unwrap_or(0);
        value = value
            .checked_mul(10)
            .and_then(|v| v.checked_add(digit))
            .ok_or_else(|| RegexError::new(offset, RegexErrorKind::RepeatTooLarge { count: usize::MAX, limit }))?;
        if value > limit {
            return Err(RegexError::new(offset, RegexErrorKind::RepeatTooLarge { count: value, limit }));
        }
        count = count.saturating_add(1);
    }
    if count == 0 {
        return Err(RegexError::new(offset, RegexErrorKind::InvalidQuantifier));
    }
    Ok(value)
}

/// Parse an escape sequence outside a character class after consuming `\`.
fn parse_escape_outside<A: Allocator>(
    cursor: &mut Cursor<'_>,
    esc_offset: usize,
    flags: Flags,
    nodes: &mut Vec<Node, A>,
    ranges: &mut Vec<CharRange, A>,
    plant: RegexPlant,
) -> Result<NodeId, RegexError> {
    let Some((_, ch)) = cursor.next_char() else {
        return Err(RegexError::new(esc_offset, RegexErrorKind::InvalidEscape));
    };
    match ch {
        'A' => push_node(nodes, Node::Anchor(AnchorKind::StartText), esc_offset),
        'z' => push_node(nodes, Node::Anchor(AnchorKind::EndText), esc_offset),
        'b' => push_node(nodes, Node::Anchor(AnchorKind::WordBoundary), esc_offset),
        'B' => push_node(nodes, Node::Anchor(AnchorKind::NonWordBoundary), esc_offset),
        'd' => make_shorthand_node(Shorthand::Digit, esc_offset, nodes, ranges),
        'D' => make_shorthand_node(Shorthand::NotDigit, esc_offset, nodes, ranges),
        'w' => make_shorthand_node(Shorthand::Word, esc_offset, nodes, ranges),
        'W' => make_shorthand_node(Shorthand::NotWord, esc_offset, nodes, ranges),
        's' => make_shorthand_node(Shorthand::Space, esc_offset, nodes, ranges),
        'S' => make_shorthand_node(Shorthand::NotSpace, esc_offset, nodes, ranges),
        _ => {
            let lit = parse_char_escape_body(cursor, esc_offset, ch)?;
            let ci = flags.case_insensitive && plant != RegexPlant::IgnoreCaseFlag;
            push_node(nodes, Node::Literal { ch: lit, case_insensitive: ci }, esc_offset)
        }
    }
}

fn make_shorthand_node<A: Allocator>(
    kind: Shorthand,
    offset: usize,
    nodes: &mut Vec<Node, A>,
    ranges: &mut Vec<CharRange, A>,
) -> Result<NodeId, RegexError> {
    let start_idx = ranges.len();
    push_shorthand_ranges(kind, ranges)?;
    let range_start = u32::try_from(start_idx).map_err(|_| {
        RegexError::new(offset, RegexErrorKind::TooManyStates { limit: usize::try_from(u32::MAX).unwrap_or(usize::MAX) })
    })?;
    let range_len = u32::try_from(ranges.len().saturating_sub(start_idx)).map_err(|_| {
        RegexError::new(offset, RegexErrorKind::TooManyStates { limit: usize::try_from(u32::MAX).unwrap_or(usize::MAX) })
    })?;
    push_node(nodes, Node::Class { range_start, range_len, negated: false }, offset)
}

fn parse_char_escape_body(
    cursor: &mut Cursor<'_>,
    esc_offset: usize,
    ch: char,
) -> Result<char, RegexError> {
    match ch {
        'n' => Ok('\n'),
        'r' => Ok('\r'),
        't' => Ok('\t'),
        'f' => Ok('\x0C'),
        'v' => Ok('\x0B'),
        '0' => Ok('\0'),
        'x' => parse_hex_escape(cursor, esc_offset),
        c if c.is_ascii_punctuation() => Ok(c),
        _ => Err(RegexError::new(esc_offset, RegexErrorKind::InvalidEscape)),
    }
}

fn parse_hex_escape(cursor: &mut Cursor<'_>, esc_offset: usize) -> Result<char, RegexError> {
    if cursor.eat('{') {
        let mut val = 0u32;
        let mut digits = 0usize;
        while let Some((_, ch)) = cursor.next_char() {
            if ch == '}' {
                if digits == 0 {
                    return Err(RegexError::new(esc_offset, RegexErrorKind::InvalidEscape));
                }
                return char::from_u32(val)
                    .ok_or_else(|| RegexError::new(esc_offset, RegexErrorKind::InvalidEscape));
            }
            let Some(hex) = hex_value(ch) else {
                return Err(RegexError::new(esc_offset, RegexErrorKind::InvalidEscape));
            };
            digits = digits.saturating_add(1);
            if digits > 6 {
                return Err(RegexError::new(esc_offset, RegexErrorKind::InvalidEscape));
            }
            val = (val << 4) | u32::from(hex);
        }
        return Err(RegexError::new(esc_offset, RegexErrorKind::InvalidEscape));
    }

    let Some((_, hi_ch)) = cursor.next_char() else {
        return Err(RegexError::new(esc_offset, RegexErrorKind::InvalidEscape));
    };
    let Some((_, lo_ch)) = cursor.next_char() else {
        return Err(RegexError::new(esc_offset, RegexErrorKind::InvalidEscape));
    };
    let (Some(hi), Some(lo)) = (hex_value(hi_ch), hex_value(lo_ch)) else {
        return Err(RegexError::new(esc_offset, RegexErrorKind::InvalidEscape));
    };
    Ok(char::from((hi << 4) | lo))
}

#[inline]
fn hex_value(ch: char) -> Option<u8> {
    let code = u32::from(ch);
    match ch {
        '0'..='9' => u8::try_from(code.saturating_sub(u32::from('0'))).ok(),
        'a'..='f' => u8::try_from(code.saturating_sub(u32::from('a')).saturating_add(10)).ok(),
        'A'..='F' => u8::try_from(code.saturating_sub(u32::from('A')).saturating_add(10)).ok(),
        _ => None,
    }
}

/// Parse a character class `[...]` or `[^...]` after consuming `[`.
fn parse_char_class<A: Allocator>(
    cursor: &mut Cursor<'_>,
    open_offset: usize,
    flags: Flags,
    nodes: &mut Vec<Node, A>,
    ranges: &mut Vec<CharRange, A>,
    plant: RegexPlant,
) -> Result<NodeId, RegexError> {
    let negated = cursor.eat('^');
    let range_start_idx = ranges.len();
    let fold_case = flags.case_insensitive && plant != RegexPlant::IgnoreCaseFlag;
    let mut item_count = 0usize;

    loop {
        let Some(peek) = cursor.peek() else {
            return Err(RegexError::new(open_offset, RegexErrorKind::UnclosedClass));
        };
        if peek == ']' && item_count > 0 {
            let _ = cursor.next_char();
            break;
        }
        let (atom_offset, first_atom) = parse_class_atom(cursor)?;
        item_count = item_count.saturating_add(1);

        if cursor.peek() == Some('-') && cursor.peek_second().is_some_and(|c| c != ']') {
            let _ = cursor.next_char(); // consume '-'
            let (_, second_atom) = parse_class_atom(cursor)?;
            match (first_atom, second_atom) {
                (ClassAtom::Char(lo), ClassAtom::Char(hi)) => {
                    if lo > hi {
                        return Err(RegexError::new(
                            atom_offset,
                            RegexErrorKind::InvalidClassRange { start: lo, end: hi },
                        ));
                    }
                    push_char_range(ranges, lo, hi, fold_case)?;
                }
                (ClassAtom::Shorthand(_), ClassAtom::Char(_) | ClassAtom::Shorthand(_))
                | (ClassAtom::Char(_), ClassAtom::Shorthand(_)) => {
                    return Err(RegexError::new(atom_offset, RegexErrorKind::InvalidEscape));
                }
            }
        } else {
            match first_atom {
                ClassAtom::Char(c) => push_char_range(ranges, c, c, fold_case)?,
                ClassAtom::Shorthand(sk) => push_shorthand_ranges(sk, ranges)?,
            }
        }
    }

    let range_len = normalize_ranges(ranges, range_start_idx);
    let range_start = u32::try_from(range_start_idx).map_err(|_| {
        RegexError::new(
            open_offset,
            RegexErrorKind::TooManyStates { limit: usize::try_from(u32::MAX).unwrap_or(usize::MAX) },
        )
    })?;
    push_node(nodes, Node::Class { range_start, range_len, negated }, open_offset)
}

fn parse_class_atom(cursor: &mut Cursor<'_>) -> Result<(usize, ClassAtom), RegexError> {
    let Some((offset, ch)) = cursor.next_char() else {
        return Err(RegexError::new(cursor.pos, RegexErrorKind::UnclosedClass));
    };
    if ch != '\\' {
        return Ok((offset, ClassAtom::Char(ch)));
    }
    let Some((_, esc)) = cursor.next_char() else {
        return Err(RegexError::new(offset, RegexErrorKind::InvalidEscape));
    };
    match esc {
        'd' => Ok((offset, ClassAtom::Shorthand(Shorthand::Digit))),
        'D' => Ok((offset, ClassAtom::Shorthand(Shorthand::NotDigit))),
        'w' => Ok((offset, ClassAtom::Shorthand(Shorthand::Word))),
        'W' => Ok((offset, ClassAtom::Shorthand(Shorthand::NotWord))),
        's' => Ok((offset, ClassAtom::Shorthand(Shorthand::Space))),
        'S' => Ok((offset, ClassAtom::Shorthand(Shorthand::NotSpace))),
        _ => {
            let lit = parse_char_escape_body(cursor, offset, esc)?;
            Ok((offset, ClassAtom::Char(lit)))
        }
    }
}

fn push_char_range<A: Allocator>(
    ranges: &mut Vec<CharRange, A>,
    lo: char,
    hi: char,
    fold_case: bool,
) -> Result<(), RegexError> {
    ranges.try_push(CharRange { start: lo, end: hi }).map_err(|r| RegexError::alloc(r.why))?;
    if !fold_case {
        return Ok(());
    }
    let low_lo = lo.max('a');
    let low_hi = hi.min('z');
    if low_lo <= low_hi {
        ranges
            .try_push(CharRange {
                start: low_lo.to_ascii_uppercase(),
                end: low_hi.to_ascii_uppercase(),
            })
            .map_err(|r| RegexError::alloc(r.why))?;
    }
    let up_lo = lo.max('A');
    let up_hi = hi.min('Z');
    if up_lo <= up_hi {
        ranges
            .try_push(CharRange {
                start: up_lo.to_ascii_lowercase(),
                end: up_hi.to_ascii_lowercase(),
            })
            .map_err(|r| RegexError::alloc(r.why))?;
    }
    Ok(())
}

fn push_shorthand_ranges<A: Allocator>(
    kind: Shorthand,
    ranges: &mut Vec<CharRange, A>,
) -> Result<(), RegexError> {
    const DIGIT: &[CharRange] = &[CharRange { start: '0', end: '9' }];
    const NOT_DIGIT: &[CharRange] = &[
        CharRange { start: '\0', end: '/' },
        CharRange { start: ':', end: char::MAX },
    ];
    const WORD: &[CharRange] = &[
        CharRange { start: '0', end: '9' },
        CharRange { start: 'A', end: 'Z' },
        CharRange { start: '_', end: '_' },
        CharRange { start: 'a', end: 'z' },
    ];
    const NOT_WORD: &[CharRange] = &[
        CharRange { start: '\0', end: '/' },
        CharRange { start: ':', end: '@' },
        CharRange { start: '[', end: '^' },
        CharRange { start: '`', end: '`' },
        CharRange { start: '{', end: char::MAX },
    ];
    const SPACE: &[CharRange] = &[
        CharRange { start: '\t', end: '\r' },
        CharRange { start: ' ', end: ' ' },
    ];
    const NOT_SPACE: &[CharRange] = &[
        CharRange { start: '\0', end: '\x08' },
        CharRange { start: '\x0E', end: '\x1F' },
        CharRange { start: '!', end: char::MAX },
    ];

    let slice = match kind {
        Shorthand::Digit => DIGIT,
        Shorthand::NotDigit => NOT_DIGIT,
        Shorthand::Word => WORD,
        Shorthand::NotWord => NOT_WORD,
        Shorthand::Space => SPACE,
        Shorthand::NotSpace => NOT_SPACE,
    };
    ranges.try_extend_from_slice(slice).map_err(RegexError::alloc)
}

fn normalize_ranges<A: Allocator>(ranges: &mut Vec<CharRange, A>, start: usize) -> u32 {
    let Some(slice) = ranges.get_mut(start..) else {
        return 0;
    };
    if slice.len() <= 1 {
        return u32::try_from(slice.len()).unwrap_or(0);
    }
    slice.sort_unstable_by_key(|r| r.start);
    let mut write_idx = 0usize;
    let total = slice.len();
    for read_idx in 1..total {
        let Some(curr) = slice.get(read_idx).copied() else {
            break;
        };
        let Some(prev) = slice.get_mut(write_idx) else {
            break;
        };
        if u32::from(prev.end).saturating_add(1) >= u32::from(curr.start) {
            if curr.end > prev.end {
                prev.end = curr.end;
            }
        } else {
            write_idx = write_idx.saturating_add(1);
            if let Some(dst) = slice.get_mut(write_idx) {
                *dst = curr;
            }
        }
    }
    let new_len = write_idx.saturating_add(1);
    ranges.truncate(start.saturating_add(new_len));
    u32::try_from(new_len).unwrap_or(0)
}
