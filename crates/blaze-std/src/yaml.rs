//! Strict, bounded, zero-recursion `#![no_std]` YAML 1.2 Core Schema parser and writer over
//! [`blaze_data::Value`].
//!
//! - Uses explicit heap stacks ([`Vec`]) for both parsing ([`read_in`]) and writing
//!   ([`write_into`], [`to_string_in`]) — zero recursive functions anywhere.
//! - Implements the YAML 1.2 Core Schema (so `NO`, `yes`, `on`, `off` remain text strings,
//!   avoiding the YAML 1.1 Norway problem), block & flow mappings and sequences, compact
//!   nested items (`- key: val`, `- - item`), single/double quoted strings with full YAML 1.2
//!   escapes, literal (`|`, `|-`, `|+`) and folded (`>`, `>-`, `>+`) block scalars, `#` comments,
//!   and `---` / `...` document markers.
//! - Refuses duplicate mapping keys and enforces [`YamlLimits`] (`most_bytes`, `most_depth`,
//!   `most_nodes`).
//! - Zero `unsafe`, zero integer `as` casts, zero `.clone()`, zero panicking slice indexing.

use core::alloc::Allocator;
use core::fmt;

use blaze_data::{Int, Map, Value};
use blaze_own::{AllocFailure, String, Vec};

/// Bounds enforced while parsing untrusted YAML text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct YamlLimits {
    /// Maximum input size in bytes.
    pub most_bytes: usize,
    /// Maximum container nesting depth.
    pub most_depth: usize,
    /// Maximum total YAML value nodes constructed.
    pub most_nodes: usize,
}

impl YamlLimits {
    /// Generous default bounds (16 MiB, depth 64, 1,000,000 nodes).
    pub const USUAL: Self = Self {
        most_bytes: 16 << 20,
        most_depth: 64,
        most_nodes: 1_000_000,
    };

    /// Alias matching `blaze_json::Limits::SHIPPED`.
    pub const SHIPPED: Self = Self::USUAL;
}

/// Why parsing YAML failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct YamlError {
    /// 1-based line number.
    pub line: usize,
    /// 1-based column number.
    pub col: usize,
    /// Byte offset in the input text.
    pub offset: usize,
    /// Specific failure reason.
    pub why: YamlWhy,
}

/// Failure category for [`YamlError`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum YamlWhy {
    InputTooLarge { bytes: usize, limit: usize },
    TooDeep { limit: usize },
    TooManyNodes { limit: usize },
    DuplicateKey,
    UnclosedQuote,
    InvalidEscape,
    InvalidUnicodeEscape,
    UnclosedFlow,
    UnexpectedToken,
    InvalidIndentation,
    TabIndentation,
    InvalidBlockScalarHeader,
    TrailingContent,
    Alloc(AllocFailure),
}

impl fmt::Display for YamlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "bstd::yaml: line {}, col {} (byte {}): ",
            self.line, self.col, self.offset
        )?;
        match self.why {
            YamlWhy::InputTooLarge { bytes, limit } => {
                write!(f, "input size {bytes} bytes exceeds limit {limit}")
            }
            YamlWhy::TooDeep { limit } => {
                write!(f, "nesting depth exceeds limit {limit}")
            }
            YamlWhy::TooManyNodes { limit } => {
                write!(f, "node count exceeds limit {limit}")
            }
            YamlWhy::DuplicateKey => f.write_str("duplicate mapping key"),
            YamlWhy::UnclosedQuote => f.write_str("unterminated quoted string"),
            YamlWhy::InvalidEscape => f.write_str("invalid escape sequence in double-quoted string"),
            YamlWhy::InvalidUnicodeEscape => {
                f.write_str("invalid unicode scalar escape in double-quoted string")
            }
            YamlWhy::UnclosedFlow => f.write_str("unterminated flow sequence or mapping"),
            YamlWhy::UnexpectedToken => f.write_str("unexpected token"),
            YamlWhy::InvalidIndentation => f.write_str("invalid indentation"),
            YamlWhy::TabIndentation => f.write_str("tabs are not permitted for block indentation"),
            YamlWhy::InvalidBlockScalarHeader => f.write_str("invalid block scalar header"),
            YamlWhy::TrailingContent => f.write_str("unexpected trailing content"),
            YamlWhy::Alloc(err) => write!(f, "allocation failed ({err})"),
        }
    }
}

impl core::error::Error for YamlError {}

/// Why serializing a [`Value`] to YAML failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum YamlWriteError {
    /// YAML 1.2 Core Schema has no native binary value representation without tags.
    UnsupportedBytes,
    /// Underlying [`fmt::Write`] sink returned an error.
    Fmt,
    /// Caller's allocator refused an allocation for the iterative traversal stack.
    Alloc(AllocFailure),
}

impl fmt::Display for YamlWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            YamlWriteError::UnsupportedBytes => {
                f.write_str("bstd::yaml: Value::Bytes cannot be encoded in YAML 1.2 Core Schema")
            }
            YamlWriteError::Fmt => f.write_str("bstd::yaml: output writer returned fmt::Error"),
            YamlWriteError::Alloc(err) => write!(f, "bstd::yaml: allocation failed ({err})"),
        }
    }
}

impl core::error::Error for YamlWriteError {}

/// Parse YAML 1.2 Core Schema text into a [`Value`] allocated in `alloc`.
pub fn read_in<A: Allocator + Copy>(
    input: &str,
    limits: YamlLimits,
    alloc: A,
) -> Result<Value<A>, YamlError> {
    if input.len() > limits.most_bytes {
        return Err(YamlError {
            line: 1,
            col: 1,
            offset: 0,
            why: YamlWhy::InputTooLarge { bytes: input.len(), limit: limits.most_bytes },
        });
    }

    let start_pos = if input.starts_with('\u{FEFF}') { 3usize } else { 0usize };
    let mut parser = Parser {
        cur: Cursor {
            input,
            bytes: input.as_bytes(),
            pos: start_pos,
            line: 1,
            col: 1,
        },
        limits,
        alloc,
        nodes: 0,
        stack: Vec::new_in(alloc),
        root: None,
    };
    parser.parse()
}

/// Write `value` as YAML 1.2 Core Schema text into `out` using an iterative stack in `alloc`.
pub fn write_into<A: Allocator + Copy, W: fmt::Write>(
    value: &Value<A>,
    alloc: A,
    out: &mut W,
) -> Result<(), YamlWriteError> {
    write_into_planted(value, alloc, out, false)
}

/// Serialize `value` into a new YAML 1.2 Core Schema [`String`] in `alloc`.
pub fn to_string_in<A: Allocator + Copy>(
    value: &Value<A>,
    alloc: A,
) -> Result<String<A>, YamlWriteError> {
    to_string_in_planted(value, alloc, false)
}

fn to_string_in_planted<A: Allocator + Copy>(
    value: &Value<A>,
    alloc: A,
    plant_skip_keyword_quote: bool,
) -> Result<String<A>, YamlWriteError> {
    let mut out = String::new_in(alloc);
    write_into_planted(value, alloc, &mut out, plant_skip_keyword_quote)?;
    Ok(out)
}

// ================================================================================================
// Cursor
// ================================================================================================

#[derive(Clone, Copy)]
struct Cursor<'a> {
    input: &'a str,
    bytes: &'a [u8],
    pos: usize,
    line: usize,
    col: usize,
}

impl<'a> Cursor<'a> {
    #[inline]
    fn is_eof(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    #[inline]
    fn indent(&self) -> usize {
        self.col.saturating_sub(1)
    }

    #[inline]
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    #[inline]
    fn peek_at(&self, delta: usize) -> Option<u8> {
        self.bytes.get(self.pos.checked_add(delta)?).copied()
    }

    #[inline]
    fn bump_byte(&mut self) -> Option<u8> {
        let b = *self.bytes.get(self.pos)?;
        self.pos += 1;
        if b == b'\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(b)
    }

    #[inline]
    fn bump_newline(&mut self) -> bool {
        match self.peek() {
            Some(b'\n') => {
                self.bump_byte();
                true
            }
            Some(b'\r') => {
                self.bump_byte();
                if self.peek() == Some(b'\n') {
                    self.bump_byte();
                } else {
                    self.line += 1;
                    self.col = 1;
                }
                true
            }
            _ => false,
        }
    }

    #[inline]
    fn skip_hspace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.bump_byte();
        }
    }

    #[inline]
    fn skip_hspace_any(&mut self) -> bool {
        let mut skipped = false;
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.bump_byte();
            skipped = true;
        }
        skipped
    }

    #[inline]
    fn skip_to_eol(&mut self) {
        while let Some(b) = self.peek() {
            if b == b'\n' || b == b'\r' {
                self.bump_newline();
                return;
            }
            self.bump_byte();
        }
    }

    #[inline]
    fn err(&self, why: YamlWhy) -> YamlError {
        YamlError {
            line: self.line,
            col: self.col,
            offset: self.pos,
            why,
        }
    }

    #[inline]
    fn slice(&self, start: usize, end: usize) -> &'a str {
        self.input.get(start..end).unwrap_or("")
    }

    #[inline]
    fn at_seq_entry(&self) -> bool {
        self.peek() == Some(b'-')
            && matches!(self.peek_at(1), None | Some(b' ' | b'\t' | b'\r' | b'\n'))
    }

    #[inline]
    fn at_marker(&self, marker: [u8; 3]) -> bool {
        let [m0, m1, m2] = marker;
        self.peek() == Some(m0)
            && self.peek_at(1) == Some(m1)
            && self.peek_at(2) == Some(m2)
            && matches!(self.peek_at(3), None | Some(b' ' | b'\t' | b'\r' | b'\n'))
    }
}

#[inline]
fn at_marker_bytes(bytes: &[u8], pos: usize, marker: [u8; 3]) -> bool {
    let [m0, m1, m2] = marker;
    bytes.get(pos) == Some(&m0)
        && bytes.get(pos + 1) == Some(&m1)
        && bytes.get(pos + 2) == Some(&m2)
        && matches!(bytes.get(pos + 3), None | Some(&b' ' | &b'\t' | &b'\r' | &b'\n'))
}

// ================================================================================================
// Iterative Parser State Machine
// ================================================================================================

enum Frame<A: Allocator> {
    BlockSeq {
        indent: usize,
        items: Vec<Value<A>, A>,
        pending_item: bool,
    },
    BlockMap {
        indent: usize,
        map: Map<A>,
        pending_key: Option<String<A>>,
    },
    FlowSeq {
        items: Vec<Value<A>, A>,
        expect_value: bool,
    },
    FlowMap {
        map: Map<A>,
        pending_key: Option<String<A>>,
        expect_entry: bool,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockStyle {
    Literal,
    Folded,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Chomp {
    Clip,
    Strip,
    Keep,
}

struct Parser<'a, A: Allocator + Copy> {
    cur: Cursor<'a>,
    limits: YamlLimits,
    alloc: A,
    nodes: usize,
    stack: Vec<Frame<A>, A>,
    root: Option<Value<A>>,
}

impl<'a, A: Allocator + Copy> Parser<'a, A> {
    fn parse(&mut self) -> Result<Value<A>, YamlError> {
        let mut doc_started = false;

        loop {
            if self.in_flow() {
                self.step_flow()?;
                continue;
            }

            self.skip_blank_and_comment_lines()?;
            if self.cur.is_eof() {
                break;
            }

            let line_indent = self.cur.indent();

            if line_indent == 0 && self.cur.at_marker(*b"...") {
                self.cur.bump_byte();
                self.cur.bump_byte();
                self.cur.bump_byte();
                self.finish_inline_tail()?;
                break;
            }

            if line_indent == 0 && self.cur.at_marker(*b"---") {
                if doc_started || !self.stack.is_empty() || self.root.is_some() {
                    return Err(self.cur.err(YamlWhy::TrailingContent));
                }
                doc_started = true;
                self.cur.bump_byte();
                self.cur.bump_byte();
                self.cur.bump_byte();
                self.cur.skip_hspace();
                match self.cur.peek() {
                    None | Some(b'\n' | b'\r' | b'#') => {
                        self.finish_inline_tail()?;
                    }
                    Some(_) => {
                        self.parse_inline_value()?;
                    }
                }
                continue;
            }

            doc_started = true;

            if self.root.is_some() {
                return Err(self.cur.err(YamlWhy::TrailingContent));
            }

            let is_seq_dash = self.cur.at_seq_entry();

            // Settle and unwind block frames whose scope ended at `line_indent`.
            loop {
                match self.stack.last() {
                    None => break,
                    Some(Frame::BlockMap { indent, pending_key, .. }) => {
                        let frame_indent = *indent;
                        let has_pending = pending_key.is_some();
                        if has_pending {
                            if line_indent > frame_indent
                                || (line_indent == frame_indent && is_seq_dash)
                            {
                                break;
                            }
                            self.count_node()?;
                            self.deliver_value(Value::Null)?;
                        } else if line_indent < frame_indent {
                            self.pop_block_frame()?;
                        } else if line_indent == frame_indent {
                            break;
                        } else {
                            return Err(self.cur.err(YamlWhy::InvalidIndentation));
                        }
                    }
                    Some(Frame::BlockSeq { indent, pending_item, .. }) => {
                        let frame_indent = *indent;
                        let has_pending = *pending_item;
                        if has_pending {
                            if line_indent > frame_indent {
                                break;
                            }
                            self.count_node()?;
                            self.deliver_value(Value::Null)?;
                        } else if line_indent < frame_indent {
                            self.pop_block_frame()?;
                        } else if line_indent == frame_indent {
                            if is_seq_dash {
                                break;
                            }
                            self.pop_block_frame()?;
                        } else {
                            return Err(self.cur.err(YamlWhy::InvalidIndentation));
                        }
                    }
                    Some(Frame::FlowSeq { .. } | Frame::FlowMap { .. }) => break,
                }
            }

            if self.root.is_some() {
                return Err(self.cur.err(YamlWhy::TrailingContent));
            }

            // Determine whether top frame expects a sibling item/key or a brand-new value.
            enum Need {
                SeqNext,
                MapKey,
                NewValue,
            }
            let need = match self.stack.last() {
                Some(Frame::BlockSeq { pending_item: false, .. }) => Need::SeqNext,
                Some(Frame::BlockMap { pending_key: None, .. }) => Need::MapKey,
                None
                | Some(
                    Frame::BlockSeq { pending_item: true, .. }
                    | Frame::BlockMap { pending_key: Some(_), .. }
                    | Frame::FlowSeq { .. }
                    | Frame::FlowMap { .. },
                ) => Need::NewValue,
            };

            match need {
                Need::SeqNext => {
                    self.cur.bump_byte(); // consume '-'
                    if let Some(Frame::BlockSeq { pending_item, .. }) = self.stack.last_mut() {
                        *pending_item = true;
                    }
                    self.cur.skip_hspace();
                    self.parse_after_dash()?;
                }
                Need::MapKey => {
                    let key_err_pos = self.cur;
                    let key = self.parse_block_map_key()?;
                    self.set_block_map_pending_key(key, key_err_pos)?;
                    self.cur.skip_hspace();
                    self.parse_inline_value()?;
                }
                Need::NewValue => {
                    self.parse_new_block_value()?;
                }
            }
        }

        // Unwind any remaining open block containers at EOF / `...`.
        while let Some(top) = self.stack.last() {
            match top {
                Frame::BlockSeq { .. } | Frame::BlockMap { .. } => {
                    self.pop_block_frame()?;
                }
                Frame::FlowSeq { .. } | Frame::FlowMap { .. } => {
                    return Err(self.cur.err(YamlWhy::UnclosedFlow));
                }
            }
        }

        Ok(match self.root.take() {
            Some(v) => v,
            None => Value::Null,
        })
    }

    #[inline]
    fn in_flow(&self) -> bool {
        matches!(self.stack.last(), Some(Frame::FlowSeq { .. } | Frame::FlowMap { .. }))
    }

    #[inline]
    fn current_block_indent(&self) -> Option<usize> {
        match self.stack.last() {
            Some(Frame::BlockSeq { indent, .. } | Frame::BlockMap { indent, .. }) => Some(*indent),
            None | Some(Frame::FlowSeq { .. } | Frame::FlowMap { .. }) => None,
        }
    }

    fn count_node(&mut self) -> Result<(), YamlError> {
        self.nodes += 1;
        if self.nodes > self.limits.most_nodes {
            return Err(self.cur.err(YamlWhy::TooManyNodes { limit: self.limits.most_nodes }));
        }
        Ok(())
    }

    fn push_frame(&mut self, frame: Frame<A>) -> Result<(), YamlError> {
        if self.stack.len() >= self.limits.most_depth {
            return Err(self.cur.err(YamlWhy::TooDeep { limit: self.limits.most_depth }));
        }
        self.count_node()?;
        self.stack
            .try_push(frame)
            .map_err(|r| self.cur.err(YamlWhy::Alloc(r.why)))?;
        Ok(())
    }

    fn deliver_value(&mut self, val: Value<A>) -> Result<(), YamlError> {
        match self.stack.last_mut() {
            None => {
                if self.root.is_some() {
                    return Err(self.cur.err(YamlWhy::TrailingContent));
                }
                self.root = Some(val);
            }
            Some(Frame::BlockSeq { items, pending_item, .. }) => {
                items
                    .try_push(val)
                    .map_err(|r| self.cur.err(YamlWhy::Alloc(r.why)))?;
                *pending_item = false;
            }
            Some(Frame::BlockMap { map, pending_key, .. }) => {
                let Some(key) = pending_key.take() else {
                    return Err(self.cur.err(YamlWhy::UnexpectedToken));
                };
                map.insert_new(key, val)
                    .map_err(|_| self.cur.err(YamlWhy::DuplicateKey))?;
            }
            Some(Frame::FlowSeq { items, expect_value }) => {
                items
                    .try_push(val)
                    .map_err(|r| self.cur.err(YamlWhy::Alloc(r.why)))?;
                *expect_value = false;
            }
            Some(Frame::FlowMap { map, pending_key, expect_entry }) => {
                let Some(key) = pending_key.take() else {
                    return Err(self.cur.err(YamlWhy::UnexpectedToken));
                };
                map.insert_new(key, val)
                    .map_err(|_| self.cur.err(YamlWhy::DuplicateKey))?;
                *expect_entry = false;
            }
        }
        Ok(())
    }

    fn pop_block_frame(&mut self) -> Result<(), YamlError> {
        let Some(frame) = self.stack.pop() else {
            return Ok(());
        };
        match frame {
            Frame::BlockSeq { mut items, pending_item, .. } => {
                if pending_item {
                    self.count_node()?;
                    items
                        .try_push(Value::Null)
                        .map_err(|r| self.cur.err(YamlWhy::Alloc(r.why)))?;
                }
                self.deliver_value(Value::List(items))
            }
            Frame::BlockMap { mut map, pending_key, .. } => {
                if let Some(key) = pending_key {
                    self.count_node()?;
                    map.insert_new(key, Value::Null)
                        .map_err(|_| self.cur.err(YamlWhy::DuplicateKey))?;
                }
                self.deliver_value(Value::Map(map))
            }
            Frame::FlowSeq { .. } | Frame::FlowMap { .. } => {
                Err(self.cur.err(YamlWhy::UnclosedFlow))
            }
        }
    }

    fn set_block_map_pending_key(
        &mut self,
        key: String<A>,
        key_pos: Cursor<'a>,
    ) -> Result<(), YamlError> {
        match self.stack.last_mut() {
            Some(Frame::BlockMap { map, pending_key, .. }) => {
                if map.contains_key(key.as_str()) {
                    return Err(key_pos.err(YamlWhy::DuplicateKey));
                }
                *pending_key = Some(key);
                Ok(())
            }
            None | Some(Frame::BlockSeq { .. } | Frame::FlowSeq { .. } | Frame::FlowMap { .. }) => {
                Err(key_pos.err(YamlWhy::UnexpectedToken))
            }
        }
    }

    fn skip_blank_and_comment_lines(&mut self) -> Result<(), YamlError> {
        loop {
            while self.cur.peek() == Some(b' ') {
                self.cur.bump_byte();
            }
            match self.cur.peek() {
                None => return Ok(()),
                Some(b'\n' | b'\r') => {
                    self.cur.bump_newline();
                }
                Some(b'#') => {
                    self.cur.skip_to_eol();
                }
                Some(b'\t') => {
                    // Tabs are allowed on whitespace-only or comment-only lines, never as indentation.
                    let tab_pos = self.cur;
                    self.cur.skip_hspace();
                    match self.cur.peek() {
                        None => return Ok(()),
                        Some(b'\n' | b'\r') => {
                            self.cur.bump_newline();
                        }
                        Some(b'#') => {
                            self.cur.skip_to_eol();
                        }
                        Some(_) => {
                            return Err(tab_pos.err(YamlWhy::TabIndentation));
                        }
                    }
                }
                Some(_) => return Ok(()),
            }
        }
    }

    fn finish_inline_tail(&mut self) -> Result<(), YamlError> {
        self.cur.skip_hspace();
        match self.cur.peek() {
            None => Ok(()),
            Some(b'\n' | b'\r') => {
                self.cur.bump_newline();
                Ok(())
            }
            Some(b'#') => {
                self.cur.skip_to_eol();
                Ok(())
            }
            Some(_) => Err(self.cur.err(YamlWhy::TrailingContent)),
        }
    }

    fn parse_new_block_value(&mut self) -> Result<(), YamlError> {
        if self.cur.at_seq_entry() {
            let start_indent = self.cur.indent();
            self.push_frame(Frame::BlockSeq {
                indent: start_indent,
                items: Vec::new_in(self.alloc),
                pending_item: true,
            })?;
            self.cur.bump_byte(); // '-'
            self.cur.skip_hspace();
            return self.parse_after_dash();
        }
        self.parse_non_seq_block_value()
    }

    fn parse_after_dash(&mut self) -> Result<(), YamlError> {
        loop {
            match self.cur.peek() {
                None | Some(b'\n' | b'\r' | b'#') => {
                    return self.finish_inline_tail();
                }
                _ => {}
            }
            if self.cur.at_seq_entry() {
                let sub_indent = self.cur.indent();
                self.push_frame(Frame::BlockSeq {
                    indent: sub_indent,
                    items: Vec::new_in(self.alloc),
                    pending_item: true,
                })?;
                self.cur.bump_byte(); // '-'
                self.cur.skip_hspace();
                continue;
            }
            return self.parse_non_seq_block_value();
        }
    }

    fn parse_non_seq_block_value(&mut self) -> Result<(), YamlError> {
        let start_indent = self.cur.indent();
        match self.cur.peek() {
            Some(b'[') => {
                self.cur.bump_byte();
                self.push_frame(Frame::FlowSeq {
                    items: Vec::new_in(self.alloc),
                    expect_value: true,
                })
            }
            Some(b'{') => {
                self.cur.bump_byte();
                self.push_frame(Frame::FlowMap {
                    map: Map::new_in(self.alloc),
                    pending_key: None,
                    expect_entry: true,
                })
            }
            Some(b'|') => {
                let text = self.parse_block_scalar(BlockStyle::Literal)?;
                self.count_node()?;
                self.deliver_value(Value::Text(text))
            }
            Some(b'>') => {
                let text = self.parse_block_scalar(BlockStyle::Folded)?;
                self.count_node()?;
                self.deliver_value(Value::Text(text))
            }
            Some(b'"' | b'\'') => {
                let text = if self.cur.peek() == Some(b'"') {
                    self.parse_double_quoted()?
                } else {
                    self.parse_single_quoted()?
                };
                if self.consume_mapping_colon_after_quoted() {
                    self.push_frame(Frame::BlockMap {
                        indent: start_indent,
                        map: Map::new_in(self.alloc),
                        pending_key: Some(text),
                    })?;
                    self.cur.skip_hspace();
                    self.parse_inline_value()
                } else {
                    self.finish_inline_tail()?;
                    self.count_node()?;
                    self.deliver_value(Value::Text(text))
                }
            }
            Some(_) => {
                if let Some(colon_pos) = self.scan_plain_mapping_colon() {
                    let raw = self.cur.slice(self.cur.pos, colon_pos);
                    let trimmed = raw.trim_end_matches([' ', '\t']);
                    let key = String::try_from_str_in(trimmed, self.alloc)
                        .map_err(|e| self.cur.err(YamlWhy::Alloc(e)))?;
                    self.advance_to(colon_pos + 1);
                    self.push_frame(Frame::BlockMap {
                        indent: start_indent,
                        map: Map::new_in(self.alloc),
                        pending_key: Some(key),
                    })?;
                    self.cur.skip_hspace();
                    self.parse_inline_value()
                } else {
                    let val = self.parse_plain_block_scalar()?;
                    self.count_node()?;
                    self.deliver_value(val)
                }
            }
            None => Ok(()),
        }
    }

    fn parse_inline_value(&mut self) -> Result<(), YamlError> {
        match self.cur.peek() {
            None | Some(b'\n' | b'\r' | b'#') => self.finish_inline_tail(),
            Some(b'[') => {
                self.cur.bump_byte();
                self.push_frame(Frame::FlowSeq {
                    items: Vec::new_in(self.alloc),
                    expect_value: true,
                })
            }
            Some(b'{') => {
                self.cur.bump_byte();
                self.push_frame(Frame::FlowMap {
                    map: Map::new_in(self.alloc),
                    pending_key: None,
                    expect_entry: true,
                })
            }
            Some(b'|') => {
                let text = self.parse_block_scalar(BlockStyle::Literal)?;
                self.count_node()?;
                self.deliver_value(Value::Text(text))
            }
            Some(b'>') => {
                let text = self.parse_block_scalar(BlockStyle::Folded)?;
                self.count_node()?;
                self.deliver_value(Value::Text(text))
            }
            Some(b'"') => {
                let text = self.parse_double_quoted()?;
                self.finish_inline_tail()?;
                self.count_node()?;
                self.deliver_value(Value::Text(text))
            }
            Some(b'\'') => {
                let text = self.parse_single_quoted()?;
                self.finish_inline_tail()?;
                self.count_node()?;
                self.deliver_value(Value::Text(text))
            }
            Some(_) => {
                if self.cur.at_seq_entry() {
                    return Err(self.cur.err(YamlWhy::UnexpectedToken));
                }
                if self.scan_plain_mapping_colon().is_some() {
                    return Err(self.cur.err(YamlWhy::UnexpectedToken));
                }
                let val = self.parse_plain_block_scalar()?;
                self.count_node()?;
                self.deliver_value(val)
            }
        }
    }

    fn parse_block_map_key(&mut self) -> Result<String<A>, YamlError> {
        match self.cur.peek() {
            Some(b'"') => {
                let key = self.parse_double_quoted()?;
                if !self.consume_mapping_colon_after_quoted() {
                    return Err(self.cur.err(YamlWhy::UnexpectedToken));
                }
                Ok(key)
            }
            Some(b'\'') => {
                let key = self.parse_single_quoted()?;
                if !self.consume_mapping_colon_after_quoted() {
                    return Err(self.cur.err(YamlWhy::UnexpectedToken));
                }
                Ok(key)
            }
            _ => {
                let Some(colon_pos) = self.scan_plain_mapping_colon() else {
                    return Err(self.cur.err(YamlWhy::UnexpectedToken));
                };
                let raw = self.cur.slice(self.cur.pos, colon_pos);
                let trimmed = raw.trim_end_matches([' ', '\t']);
                let key = String::try_from_str_in(trimmed, self.alloc)
                    .map_err(|e| self.cur.err(YamlWhy::Alloc(e)))?;
                self.advance_to(colon_pos + 1);
                Ok(key)
            }
        }
    }

    fn consume_mapping_colon_after_quoted(&mut self) -> bool {
        let saved = self.cur;
        self.cur.skip_hspace();
        if self.cur.peek() == Some(b':')
            && matches!(self.cur.peek_at(1), None | Some(b' ' | b'\t' | b'\r' | b'\n'))
        {
            self.cur.bump_byte();
            true
        } else {
            self.cur = saved;
            false
        }
    }

    fn scan_plain_mapping_colon(&self) -> Option<usize> {
        let bytes = self.cur.bytes;
        let mut p = self.cur.pos;
        let mut prev_ws = false;
        while let Some(&b) = bytes.get(p) {
            if b == b'\n' || b == b'\r' {
                return None;
            }
            if b == b'#' && prev_ws {
                return None;
            }
            if b == b':' && matches!(bytes.get(p + 1), None | Some(&b' ' | &b'\t' | &b'\r' | &b'\n'))
            {
                return Some(p);
            }
            prev_ws = b == b' ' || b == b'\t';
            p += 1;
        }
        None
    }

    fn advance_to(&mut self, target_pos: usize) {
        while self.cur.pos < target_pos {
            if self.cur.bump_byte().is_none() {
                break;
            }
        }
    }

    fn parse_plain_block_scalar(&mut self) -> Result<Value<A>, YamlError> {
        let start = self.cur.pos;
        let mut end = start;
        let mut prev_ws = false;
        while let Some(b) = self.cur.peek() {
            if b == b'\n' || b == b'\r' {
                self.cur.bump_newline();
                break;
            }
            if b == b'#' && prev_ws {
                self.cur.skip_to_eol();
                break;
            }
            prev_ws = b == b' ' || b == b'\t';
            self.cur.bump_byte();
            end = self.cur.pos;
        }
        let raw = self.cur.slice(start, end).trim_end_matches([' ', '\t']);
        resolve_core_scalar(raw, self.alloc).map_err(|e| self.cur.err(YamlWhy::Alloc(e)))
    }

    // --------------------------------------------------------------------------------------------
    // Flow Mode (Iterative)
    // --------------------------------------------------------------------------------------------

    fn skip_flow_ws_and_comments(&mut self) {
        loop {
            match self.cur.peek() {
                Some(b' ' | b'\t') => {
                    self.cur.bump_byte();
                }
                Some(b'\n' | b'\r') => {
                    self.cur.bump_newline();
                }
                Some(b'#') => {
                    self.cur.skip_to_eol();
                }
                _ => return,
            }
        }
    }

    fn step_flow(&mut self) -> Result<(), YamlError> {
        self.skip_flow_ws_and_comments();
        if self.cur.is_eof() {
            return Err(self.cur.err(YamlWhy::UnclosedFlow));
        }

        enum FlowKind {
            Seq { expect_value: bool },
            Map { has_pending: bool, expect_entry: bool },
        }
        let kind = match self.stack.last() {
            Some(Frame::FlowSeq { expect_value, .. }) => {
                FlowKind::Seq { expect_value: *expect_value }
            }
            Some(Frame::FlowMap { pending_key, expect_entry, .. }) => FlowKind::Map {
                has_pending: pending_key.is_some(),
                expect_entry: *expect_entry,
            },
            None | Some(Frame::BlockSeq { .. } | Frame::BlockMap { .. }) => {
                return Err(self.cur.err(YamlWhy::UnexpectedToken));
            }
        };

        match kind {
            FlowKind::Seq { expect_value } => {
                if self.cur.peek() == Some(b']') {
                    self.cur.bump_byte();
                    if let Some(Frame::FlowSeq { items, .. }) = self.stack.pop() {
                        self.deliver_value(Value::List(items))?;
                    }
                    if !self.in_flow() {
                        self.finish_inline_tail()?;
                    }
                    return Ok(());
                }
                if !expect_value {
                    if self.cur.peek() == Some(b',') {
                        self.cur.bump_byte();
                        if let Some(Frame::FlowSeq { expect_value: ev, .. }) = self.stack.last_mut()
                        {
                            *ev = true;
                        }
                        return Ok(());
                    }
                    return Err(self.cur.err(YamlWhy::UnexpectedToken));
                }
                self.parse_flow_value()
            }
            FlowKind::Map { has_pending, expect_entry } => {
                if has_pending {
                    if matches!(self.cur.peek(), Some(b',' | b'}')) {
                        self.count_node()?;
                        return self.deliver_value(Value::Null);
                    }
                    return self.parse_flow_value();
                }
                if self.cur.peek() == Some(b'}') {
                    self.cur.bump_byte();
                    if let Some(Frame::FlowMap { map, .. }) = self.stack.pop() {
                        self.deliver_value(Value::Map(map))?;
                    }
                    if !self.in_flow() {
                        self.finish_inline_tail()?;
                    }
                    return Ok(());
                }
                if !expect_entry {
                    if self.cur.peek() == Some(b',') {
                        self.cur.bump_byte();
                        if let Some(Frame::FlowMap { expect_entry: ee, .. }) = self.stack.last_mut()
                        {
                            *ee = true;
                        }
                        return Ok(());
                    }
                    return Err(self.cur.err(YamlWhy::UnexpectedToken));
                }

                let key_pos = self.cur;
                let key = self.parse_flow_key()?;
                self.skip_flow_ws_and_comments();
                if self.cur.peek() != Some(b':') {
                    return Err(self.cur.err(YamlWhy::UnexpectedToken));
                }
                self.cur.bump_byte();
                if let Some(Frame::FlowMap { map, pending_key, .. }) = self.stack.last_mut() {
                    if map.contains_key(key.as_str()) {
                        return Err(key_pos.err(YamlWhy::DuplicateKey));
                    }
                    *pending_key = Some(key);
                }
                Ok(())
            }
        }
    }

    fn parse_flow_value(&mut self) -> Result<(), YamlError> {
        match self.cur.peek() {
            Some(b'[') => {
                self.cur.bump_byte();
                self.push_frame(Frame::FlowSeq {
                    items: Vec::new_in(self.alloc),
                    expect_value: true,
                })
            }
            Some(b'{') => {
                self.cur.bump_byte();
                self.push_frame(Frame::FlowMap {
                    map: Map::new_in(self.alloc),
                    pending_key: None,
                    expect_entry: true,
                })
            }
            Some(b'"') => {
                let s = self.parse_double_quoted()?;
                self.count_node()?;
                self.deliver_value(Value::Text(s))
            }
            Some(b'\'') => {
                let s = self.parse_single_quoted()?;
                self.count_node()?;
                self.deliver_value(Value::Text(s))
            }
            Some(b',' | b']' | b'}') | None => Err(self.cur.err(YamlWhy::UnexpectedToken)),
            Some(_) => {
                let start = self.cur.pos;
                let mut end = start;
                let mut prev_ws = false;
                while let Some(b) = self.cur.peek() {
                    if matches!(b, b',' | b'[' | b']' | b'{' | b'}' | b'\r' | b'\n') {
                        break;
                    }
                    if b == b'#' && prev_ws {
                        break;
                    }
                    prev_ws = b == b' ' || b == b'\t';
                    self.cur.bump_byte();
                    end = self.cur.pos;
                }
                let raw = self.cur.slice(start, end).trim_end_matches([' ', '\t']);
                let val = resolve_core_scalar(raw, self.alloc)
                    .map_err(|e| self.cur.err(YamlWhy::Alloc(e)))?;
                self.count_node()?;
                self.deliver_value(val)
            }
        }
    }

    fn parse_flow_key(&mut self) -> Result<String<A>, YamlError> {
        match self.cur.peek() {
            Some(b'"') => self.parse_double_quoted(),
            Some(b'\'') => self.parse_single_quoted(),
            Some(b',' | b'[' | b']' | b'{' | b'}') | None => {
                Err(self.cur.err(YamlWhy::UnexpectedToken))
            }
            Some(_) => {
                let start = self.cur.pos;
                let mut end = start;
                let mut prev_ws = false;
                while let Some(b) = self.cur.peek() {
                    if matches!(b, b',' | b'[' | b']' | b'{' | b'}' | b'\r' | b'\n') {
                        break;
                    }
                    if b == b'#' && prev_ws {
                        break;
                    }
                    if b == b':'
                        && matches!(
                            self.cur.peek_at(1),
                            None | Some(b' ' | b'\t' | b'\r' | b'\n' | b',' | b'}' | b']' | b'{' | b'[')
                        )
                    {
                        break;
                    }
                    prev_ws = b == b' ' || b == b'\t';
                    self.cur.bump_byte();
                    end = self.cur.pos;
                }
                let raw = self.cur.slice(start, end).trim_end_matches([' ', '\t']);
                String::try_from_str_in(raw, self.alloc)
                    .map_err(|e| self.cur.err(YamlWhy::Alloc(e)))
            }
        }
    }

    // --------------------------------------------------------------------------------------------
    // Quoted Strings (YAML 1.2)
    // --------------------------------------------------------------------------------------------

    fn parse_single_quoted(&mut self) -> Result<String<A>, YamlError> {
        let open_pos = self.cur;
        self.cur.bump_byte(); // opening '\''
        let mut out = String::new_in(self.alloc);

        while let Some(b) = self.cur.peek() {
            if b == b'\'' {
                self.cur.bump_byte();
                if self.cur.peek() == Some(b'\'') {
                    self.cur.bump_byte();
                    out.try_push('\'')
                        .map_err(|e| self.cur.err(YamlWhy::Alloc(e)))?;
                    continue;
                }
                return Ok(out);
            }
            if b == b'\n' || b == b'\r' {
                self.fold_quoted_newline(&mut out)?;
                continue;
            }
            self.push_utf8_char(&mut out)?;
        }

        Err(open_pos.err(YamlWhy::UnclosedQuote))
    }

    fn parse_double_quoted(&mut self) -> Result<String<A>, YamlError> {
        let open_pos = self.cur;
        self.cur.bump_byte(); // opening '"'
        let mut out = String::new_in(self.alloc);

        while let Some(b) = self.cur.peek() {
            if b == b'"' {
                self.cur.bump_byte();
                return Ok(out);
            }
            if b == b'\\' {
                self.cur.bump_byte();
                let esc_pos = self.cur;
                let Some(esc) = self.cur.peek() else {
                    return Err(open_pos.err(YamlWhy::UnclosedQuote));
                };
                if esc == b'\n' || esc == b'\r' {
                    self.cur.bump_newline();
                    self.cur.skip_hspace();
                    continue;
                }
                self.cur.bump_byte();
                let ch = match esc {
                    b'0' => '\u{0000}',
                    b'a' => '\u{0007}',
                    b'b' => '\u{0008}',
                    b't' | b'\t' => '\t',
                    b'n' => '\n',
                    b'v' => '\u{000B}',
                    b'f' => '\u{000C}',
                    b'r' => '\r',
                    b'e' => '\u{001B}',
                    b' ' => ' ',
                    b'"' => '"',
                    b'/' => '/',
                    b'\\' => '\\',
                    b'N' => '\u{0085}',
                    b'_' => '\u{00A0}',
                    b'L' => '\u{2028}',
                    b'P' => '\u{2029}',
                    b'x' => self.parse_hex_escape(2, esc_pos)?,
                    b'u' => self.parse_hex_escape(4, esc_pos)?,
                    b'U' => self.parse_hex_escape(8, esc_pos)?,
                    _ => return Err(esc_pos.err(YamlWhy::InvalidEscape)),
                };
                out.try_push(ch)
                    .map_err(|e| self.cur.err(YamlWhy::Alloc(e)))?;
                continue;
            }
            if b == b'\n' || b == b'\r' {
                self.fold_quoted_newline(&mut out)?;
                continue;
            }
            self.push_utf8_char(&mut out)?;
        }

        Err(open_pos.err(YamlWhy::UnclosedQuote))
    }

    fn parse_hex_escape(&mut self, digits: usize, err_pos: Cursor<'a>) -> Result<char, YamlError> {
        let mut val = 0u32;
        for _ in 0..digits {
            let Some(b) = self.cur.bump_byte() else {
                return Err(err_pos.err(YamlWhy::InvalidUnicodeEscape));
            };
            let Some(nibble) = hex_nibble(b) else {
                return Err(err_pos.err(YamlWhy::InvalidUnicodeEscape));
            };
            val = (val << 4) | nibble;
        }
        char::from_u32(val).ok_or_else(|| err_pos.err(YamlWhy::InvalidUnicodeEscape))
    }

    fn fold_quoted_newline(&mut self, out: &mut String<A>) -> Result<(), YamlError> {
        // Trim trailing horizontal spaces accumulated before the line break.
        while matches!(out.as_bytes().last(), Some(&b' ' | &b'\t')) {
            out.pop();
        }
        let mut breaks = 0usize;
        loop {
            if !self.cur.bump_newline() {
                break;
            }
            breaks += 1;
            self.cur.skip_hspace();
        }
        if breaks == 1 {
            out.try_push(' ')
                .map_err(|e| self.cur.err(YamlWhy::Alloc(e)))?;
        } else {
            for _ in 1..breaks {
                out.try_push('\n')
                    .map_err(|e| self.cur.err(YamlWhy::Alloc(e)))?;
            }
        }
        Ok(())
    }

    fn push_utf8_char(&mut self, out: &mut String<A>) -> Result<(), YamlError> {
        let rest = self.cur.input.get(self.cur.pos..).unwrap_or("");
        let Some(ch) = rest.chars().next() else {
            return Ok(());
        };
        let len = ch.len_utf8();
        for _ in 0..len {
            self.cur.bump_byte();
        }
        out.try_push(ch)
            .map_err(|e| self.cur.err(YamlWhy::Alloc(e)))
    }

    // --------------------------------------------------------------------------------------------
    // Block Scalars (`|` and `>`)
    // --------------------------------------------------------------------------------------------

    fn parse_block_scalar(&mut self, style: BlockStyle) -> Result<String<A>, YamlError> {
        let header_pos = self.cur;
        self.cur.bump_byte(); // consume '|' or '>'

        let mut chomp: Option<Chomp> = None;
        let mut explicit_indent: Option<usize> = None;

        for _ in 0..2 {
            match self.cur.peek() {
                Some(b'-') => {
                    if chomp.is_some() {
                        return Err(header_pos.err(YamlWhy::InvalidBlockScalarHeader));
                    }
                    chomp = Some(Chomp::Strip);
                    self.cur.bump_byte();
                }
                Some(b'+') => {
                    if chomp.is_some() {
                        return Err(header_pos.err(YamlWhy::InvalidBlockScalarHeader));
                    }
                    chomp = Some(Chomp::Keep);
                    self.cur.bump_byte();
                }
                Some(b @ b'1'..=b'9') => {
                    if explicit_indent.is_some() {
                        return Err(header_pos.err(YamlWhy::InvalidBlockScalarHeader));
                    }
                    explicit_indent = Some(usize::from(b - b'0'));
                    self.cur.bump_byte();
                }
                _ => break,
            }
        }

        let saw_ws = self.cur.skip_hspace_any();
        match self.cur.peek() {
            None => {}
            Some(b'\n' | b'\r') => {
                self.cur.bump_newline();
            }
            Some(b'#') if saw_ws => {
                self.cur.skip_to_eol();
            }
            Some(_) => return Err(header_pos.err(YamlWhy::InvalidBlockScalarHeader)),
        }

        let chomp = chomp.unwrap_or(Chomp::Clip);
        let parent_indent = self.current_block_indent();
        let min_indent = match parent_indent {
            Some(p) => p + 1,
            None => 0,
        };

        let content_indent = match explicit_indent {
            Some(d) => match parent_indent {
                Some(p) => p + d,
                None => d,
            },
            None => self.detect_block_scalar_indent(parent_indent, min_indent)?,
        };

        let mut out = String::new_in(self.alloc);
        let mut has_content = false;
        let mut pending_empties = 0usize;
        let mut prev_more_indented = false;

        while !self.cur.is_eof() {
            let line_start = self.cur.pos;
            let bytes = self.cur.bytes;
            let mut sp = 0usize;
            while bytes.get(line_start + sp) == Some(&b' ') {
                sp += 1;
            }

            match bytes.get(line_start + sp) {
                None | Some(&b'\n' | &b'\r') => {
                    // Blank/spaces-only line.
                    if sp > content_indent {
                        // Extra spaces beyond content_indent on a line are content spaces.
                        self.advance_to(line_start + content_indent);
                        let text_start = self.cur.pos;
                        let text_end = line_start + sp;
                        self.advance_to(text_end);
                        self.cur.bump_newline();
                        let text = self.cur.slice(text_start, text_end);
                        self.append_block_line(
                            &mut out,
                            style,
                            text,
                            true,
                            &mut has_content,
                            &mut pending_empties,
                            &mut prev_more_indented,
                        )?;
                    } else {
                        self.advance_to(line_start + sp);
                        self.cur.bump_newline();
                        pending_empties += 1;
                    }
                }
                Some(_) => {
                    if sp < content_indent {
                        break;
                    }
                    if parent_indent.is_none()
                        && sp == 0
                        && (at_marker_bytes(bytes, line_start, *b"...")
                            || at_marker_bytes(bytes, line_start, *b"---"))
                    {
                        break;
                    }
                    self.advance_to(line_start + content_indent);
                    let text_start = self.cur.pos;
                    while let Some(b) = self.cur.peek() {
                        if b == b'\n' || b == b'\r' {
                            break;
                        }
                        self.cur.bump_byte();
                    }
                    let text_end = self.cur.pos;
                    self.cur.bump_newline();
                    let text = self.cur.slice(text_start, text_end);
                    let is_more_indented =
                        matches!(text.as_bytes().first(), Some(&b' ' | &b'\t'));
                    self.append_block_line(
                        &mut out,
                        style,
                        text,
                        is_more_indented,
                        &mut has_content,
                        &mut pending_empties,
                        &mut prev_more_indented,
                    )?;
                }
            }
        }

        match chomp {
            Chomp::Strip => {}
            Chomp::Clip => {
                if has_content {
                    out.try_push('\n')
                        .map_err(|e| self.cur.err(YamlWhy::Alloc(e)))?;
                }
            }
            Chomp::Keep => {
                if has_content {
                    out.try_push('\n')
                        .map_err(|e| self.cur.err(YamlWhy::Alloc(e)))?;
                }
                for _ in 0..pending_empties {
                    out.try_push('\n')
                        .map_err(|e| self.cur.err(YamlWhy::Alloc(e)))?;
                }
            }
        }

        Ok(out)
    }

    fn detect_block_scalar_indent(
        &self,
        parent_indent: Option<usize>,
        min_indent: usize,
    ) -> Result<usize, YamlError> {
        let bytes = self.cur.bytes;
        let mut p = self.cur.pos;
        let mut max_empty_spaces = 0usize;
        while p < bytes.len() {
            let line_start = p;
            let mut sp = 0usize;
            while bytes.get(p) == Some(&b' ') {
                sp += 1;
                p += 1;
            }
            match bytes.get(p) {
                None => return Ok(min_indent),
                Some(&b'\n') => {
                    max_empty_spaces = max_empty_spaces.max(sp);
                    p += 1;
                }
                Some(&b'\r') => {
                    max_empty_spaces = max_empty_spaces.max(sp);
                    p += 1;
                    if bytes.get(p) == Some(&b'\n') {
                        p += 1;
                    }
                }
                Some(_) => {
                    if sp < min_indent {
                        return Ok(min_indent);
                    }
                    if parent_indent.is_none()
                        && sp == 0
                        && (at_marker_bytes(bytes, line_start, *b"...")
                            || at_marker_bytes(bytes, line_start, *b"---"))
                    {
                        return Ok(min_indent);
                    }
                    if max_empty_spaces > sp {
                        return Err(self.cur.err(YamlWhy::InvalidIndentation));
                    }
                    return Ok(sp);
                }
            }
        }
        Ok(min_indent)
    }

    fn append_block_line(
        &self,
        out: &mut String<A>,
        style: BlockStyle,
        text: &str,
        is_more_indented: bool,
        has_content: &mut bool,
        pending_empties: &mut usize,
        prev_more_indented: &mut bool,
    ) -> Result<(), YamlError> {
        if !*has_content {
            for _ in 0..*pending_empties {
                out.try_push('\n')
                    .map_err(|e| self.cur.err(YamlWhy::Alloc(e)))?;
            }
        } else {
            match style {
                BlockStyle::Literal => {
                    for _ in 0..(*pending_empties + 1) {
                        out.try_push('\n')
                            .map_err(|e| self.cur.err(YamlWhy::Alloc(e)))?;
                    }
                }
                BlockStyle::Folded => {
                    if *prev_more_indented || is_more_indented {
                        for _ in 0..(*pending_empties + 1) {
                            out.try_push('\n')
                                .map_err(|e| self.cur.err(YamlWhy::Alloc(e)))?;
                        }
                    } else if *pending_empties == 0 {
                        out.try_push(' ')
                            .map_err(|e| self.cur.err(YamlWhy::Alloc(e)))?;
                    } else {
                        for _ in 0..*pending_empties {
                            out.try_push('\n')
                                .map_err(|e| self.cur.err(YamlWhy::Alloc(e)))?;
                        }
                    }
                }
            }
        }
        out.try_push_str(text)
            .map_err(|e| self.cur.err(YamlWhy::Alloc(e)))?;
        *has_content = true;
        *pending_empties = 0;
        *prev_more_indented = is_more_indented;
        Ok(())
    }
}

// ================================================================================================
// YAML 1.2 Core Schema Scalar Resolution
// ================================================================================================

#[inline]
fn hex_nibble(b: u8) -> Option<u32> {
    match b {
        b'0'..=b'9' => Some(u32::from(b - b'0')),
        b'a'..=b'f' => Some(u32::from(b - b'a' + 10)),
        b'A'..=b'F' => Some(u32::from(b - b'A' + 10)),
        _ => None,
    }
}

fn resolve_core_scalar<A: Allocator>(s: &str, alloc: A) -> Result<Value<A>, AllocFailure> {
    if matches!(s, "" | "null" | "Null" | "NULL" | "~") {
        return Ok(Value::Null);
    }
    if matches!(s, "true" | "True" | "TRUE") {
        return Ok(Value::Bool(true));
    }
    if matches!(s, "false" | "False" | "FALSE") {
        return Ok(Value::Bool(false));
    }

    if let Some(hex) = s.strip_prefix("0x") {
        if !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            if let Ok(n) = u64::from_str_radix(hex, 16) {
                return Ok(Value::Int(Int::from_u64(n)));
            }
        }
    }

    if let Some(oct) = s.strip_prefix("0o") {
        if !oct.is_empty() && oct.bytes().all(|b| matches!(b, b'0'..=b'7')) {
            if let Ok(n) = u64::from_str_radix(oct, 8) {
                return Ok(Value::Int(Int::from_u64(n)));
            }
        }
    }

    if is_yaml_decimal_int(s) {
        if s.starts_with('-') {
            if let Ok(n) = s.parse::<i64>() {
                return Ok(Value::Int(Int::from_i64(n)));
            }
        } else {
            let digits = s.strip_prefix('+').unwrap_or(s);
            if let Ok(n) = digits.parse::<u64>() {
                return Ok(Value::Int(Int::from_u64(n)));
            }
        }
    }

    if matches!(s, ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF") {
        return Ok(Value::Float(f64::INFINITY));
    }
    if matches!(s, "-.inf" | "-.Inf" | "-.INF") {
        return Ok(Value::Float(f64::NEG_INFINITY));
    }
    if matches!(s, ".nan" | ".NaN" | ".NAN") {
        return Ok(Value::Float(f64::NAN));
    }

    if is_yaml_float(s) {
        if let Ok(f) = s.parse::<f64>() {
            return Ok(Value::Float(f));
        }
    }

    Ok(Value::Text(String::try_from_str_in(s, alloc)?))
}

fn is_yaml_decimal_int(s: &str) -> bool {
    let digits = s
        .strip_prefix('-')
        .or_else(|| s.strip_prefix('+'))
        .unwrap_or(s);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

fn is_yaml_float(s: &str) -> bool {
    let rest = s
        .strip_prefix('-')
        .or_else(|| s.strip_prefix('+'))
        .unwrap_or(s);
    let bytes = rest.as_bytes();
    if bytes.is_empty() {
        return false;
    }

    let mut i = 0usize;
    let mut int_digits = 0usize;
    while bytes.get(i).is_some_and(u8::is_ascii_digit) {
        int_digits += 1;
        i += 1;
    }

    let mut saw_dot = false;
    let mut frac_digits = 0usize;
    if bytes.get(i) == Some(&b'.') {
        saw_dot = true;
        i += 1;
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            frac_digits += 1;
            i += 1;
        }
    }

    if int_digits == 0 && frac_digits == 0 {
        return false;
    }

    let mut saw_exp = false;
    if matches!(bytes.get(i), Some(&b'e' | &b'E')) {
        saw_exp = true;
        i += 1;
        if matches!(bytes.get(i), Some(&b'+' | &b'-')) {
            i += 1;
        }
        let mut exp_digits = 0usize;
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            exp_digits += 1;
            i += 1;
        }
        if exp_digits == 0 {
            return false;
        }
    }

    i == bytes.len() && (saw_dot || saw_exp)
}

fn plain_resolves_non_text(s: &str) -> bool {
    matches!(
        s,
        "" | "null"
            | "Null"
            | "NULL"
            | "~"
            | "true"
            | "True"
            | "TRUE"
            | "false"
            | "False"
            | "FALSE"
            | ".inf"
            | ".Inf"
            | ".INF"
            | "+.inf"
            | "+.Inf"
            | "+.INF"
            | "-.inf"
            | "-.Inf"
            | "-.INF"
            | ".nan"
            | ".NaN"
            | ".NAN"
    ) || is_yaml_decimal_int(s)
        || is_yaml_float(s)
        || s.strip_prefix("0x")
            .is_some_and(|h| !h.is_empty() && h.bytes().all(|b| b.is_ascii_hexdigit()))
        || s.strip_prefix("0o")
            .is_some_and(|o| !o.is_empty() && o.bytes().all(|b| matches!(b, b'0'..=b'7')))
}

// ================================================================================================
// Iterative Writer (Zero Recursion)
// ================================================================================================

enum WriteTask<'v, A: Allocator> {
    ListItems {
        items: &'v [Value<A>],
        idx: usize,
        indent: usize,
    },
    MapEntries {
        map: &'v Map<A>,
        idx: usize,
        indent: usize,
    },
}

fn write_into_planted<A: Allocator + Copy, W: fmt::Write>(
    value: &Value<A>,
    alloc: A,
    out: &mut W,
    plant_skip_keyword_quote: bool,
) -> Result<(), YamlWriteError> {
    let mut stack: Vec<WriteTask<'_, A>, A> = Vec::new_in(alloc);

    match value {
        Value::List(items) if !items.is_empty() => {
            stack
                .try_push(WriteTask::ListItems {
                    items: items.as_slice(),
                    idx: 0,
                    indent: 0,
                })
                .map_err(|r| YamlWriteError::Alloc(r.why))?;
        }
        Value::Map(map) if !map.is_empty() => {
            stack
                .try_push(WriteTask::MapEntries { map, idx: 0, indent: 0 })
                .map_err(|r| YamlWriteError::Alloc(r.why))?;
        }
        Value::Null
        | Value::Bool(_)
        | Value::Int(_)
        | Value::Float(_)
        | Value::Text(_)
        | Value::Bytes(_)
        | Value::List(_)
        | Value::Map(_) => {
            write_inline_value(value, out, plant_skip_keyword_quote)?;
            return Ok(());
        }
    }

    while let Some(top) = stack.last_mut() {
        match top {
            WriteTask::ListItems { items, idx, indent } => {
                let Some(item) = items.get(*idx) else {
                    stack.pop();
                    continue;
                };
                *idx += 1;
                let cur_indent = *indent;
                write_indent(cur_indent, out)?;
                out.write_char('-').map_err(|_| YamlWriteError::Fmt)?;
                match item {
                    Value::List(sub) if !sub.is_empty() => {
                        out.write_char('\n').map_err(|_| YamlWriteError::Fmt)?;
                        stack
                            .try_push(WriteTask::ListItems {
                                items: sub.as_slice(),
                                idx: 0,
                                indent: cur_indent + 2,
                            })
                            .map_err(|r| YamlWriteError::Alloc(r.why))?;
                    }
                    Value::Map(sub) if !sub.is_empty() => {
                        out.write_char('\n').map_err(|_| YamlWriteError::Fmt)?;
                        stack
                            .try_push(WriteTask::MapEntries {
                                map: sub,
                                idx: 0,
                                indent: cur_indent + 2,
                            })
                            .map_err(|r| YamlWriteError::Alloc(r.why))?;
                    }
                    Value::Null
                    | Value::Bool(_)
                    | Value::Int(_)
                    | Value::Float(_)
                    | Value::Text(_)
                    | Value::Bytes(_)
                    | Value::List(_)
                    | Value::Map(_) => {
                        out.write_char(' ').map_err(|_| YamlWriteError::Fmt)?;
                        write_inline_value(item, out, plant_skip_keyword_quote)?;
                        out.write_char('\n').map_err(|_| YamlWriteError::Fmt)?;
                    }
                }
            }
            WriteTask::MapEntries { map, idx, indent } => {
                let Some((key, val)) = map.entry_at(*idx) else {
                    stack.pop();
                    continue;
                };
                *idx += 1;
                let cur_indent = *indent;
                write_indent(cur_indent, out)?;
                write_yaml_string(key, out, plant_skip_keyword_quote)?;
                out.write_char(':').map_err(|_| YamlWriteError::Fmt)?;
                match val {
                    Value::List(sub) if !sub.is_empty() => {
                        out.write_char('\n').map_err(|_| YamlWriteError::Fmt)?;
                        stack
                            .try_push(WriteTask::ListItems {
                                items: sub.as_slice(),
                                idx: 0,
                                indent: cur_indent + 2,
                            })
                            .map_err(|r| YamlWriteError::Alloc(r.why))?;
                    }
                    Value::Map(sub) if !sub.is_empty() => {
                        out.write_char('\n').map_err(|_| YamlWriteError::Fmt)?;
                        stack
                            .try_push(WriteTask::MapEntries {
                                map: sub,
                                idx: 0,
                                indent: cur_indent + 2,
                            })
                            .map_err(|r| YamlWriteError::Alloc(r.why))?;
                    }
                    Value::Null
                    | Value::Bool(_)
                    | Value::Int(_)
                    | Value::Float(_)
                    | Value::Text(_)
                    | Value::Bytes(_)
                    | Value::List(_)
                    | Value::Map(_) => {
                        out.write_char(' ').map_err(|_| YamlWriteError::Fmt)?;
                        write_inline_value(val, out, plant_skip_keyword_quote)?;
                        out.write_char('\n').map_err(|_| YamlWriteError::Fmt)?;
                    }
                }
            }
        }
    }

    Ok(())
}

fn write_indent<W: fmt::Write>(spaces: usize, out: &mut W) -> Result<(), YamlWriteError> {
    for _ in 0..spaces {
        out.write_char(' ').map_err(|_| YamlWriteError::Fmt)?;
    }
    Ok(())
}

fn write_inline_value<A: Allocator, W: fmt::Write>(
    val: &Value<A>,
    out: &mut W,
    plant_skip_keyword_quote: bool,
) -> Result<(), YamlWriteError> {
    match val {
        Value::Null => out.write_str("null").map_err(|_| YamlWriteError::Fmt),
        Value::Bool(true) => out.write_str("true").map_err(|_| YamlWriteError::Fmt),
        Value::Bool(false) => out.write_str("false").map_err(|_| YamlWriteError::Fmt),
        Value::Int(n) => write!(out, "{n}").map_err(|_| YamlWriteError::Fmt),
        Value::Float(f) => {
            if f.is_nan() {
                out.write_str(".nan").map_err(|_| YamlWriteError::Fmt)
            } else if f.is_infinite() {
                let s = if f.is_sign_negative() { "-.inf" } else { ".inf" };
                out.write_str(s).map_err(|_| YamlWriteError::Fmt)
            } else {
                write!(out, "{f:?}").map_err(|_| YamlWriteError::Fmt)
            }
        }
        Value::Text(s) => write_yaml_string(s.as_str(), out, plant_skip_keyword_quote),
        Value::Bytes(_) => Err(YamlWriteError::UnsupportedBytes),
        Value::List(_) => out.write_str("[]").map_err(|_| YamlWriteError::Fmt),
        Value::Map(_) => out.write_str("{}").map_err(|_| YamlWriteError::Fmt),
    }
}

fn is_safe_plain_scalar(s: &str, plant_skip_keyword_quote: bool) -> bool {
    if s.is_empty() {
        return false;
    }
    let bytes = s.as_bytes();
    if matches!(bytes.first(), Some(&b' ' | &b'\t' | &b'-' | &b'.')) {
        return false;
    }
    if matches!(bytes.last(), Some(&b' ' | &b'\t')) {
        return false;
    }
    for ch in s.chars() {
        let code = u32::from(ch);
        if code < 0x20 || code == 0x7F || (0x80..=0x9F).contains(&code) || code == 0x2028 || code == 0x2029 {
            return false;
        }
        if matches!(
            ch,
            '"' | '\'' | '\\' | '#' | ':' | '[' | ']' | '{' | '}' | ',' | '&' | '*' | '!' | '|'
                | '>' | '%' | '@' | '`' | '?'
        ) {
            return false;
        }
    }
    if !plant_skip_keyword_quote && plain_resolves_non_text(s) {
        return false;
    }
    true
}

fn write_yaml_string<W: fmt::Write>(
    s: &str,
    out: &mut W,
    plant_skip_keyword_quote: bool,
) -> Result<(), YamlWriteError> {
    if is_safe_plain_scalar(s, plant_skip_keyword_quote) {
        return out.write_str(s).map_err(|_| YamlWriteError::Fmt);
    }
    out.write_char('"').map_err(|_| YamlWriteError::Fmt)?;
    for ch in s.chars() {
        match ch {
            '"' => out.write_str("\\\"").map_err(|_| YamlWriteError::Fmt)?,
            '\\' => out.write_str("\\\\").map_err(|_| YamlWriteError::Fmt)?,
            '\n' => out.write_str("\\n").map_err(|_| YamlWriteError::Fmt)?,
            '\r' => out.write_str("\\r").map_err(|_| YamlWriteError::Fmt)?,
            '\t' => out.write_str("\\t").map_err(|_| YamlWriteError::Fmt)?,
            '\u{0000}' => out.write_str("\\0").map_err(|_| YamlWriteError::Fmt)?,
            '\u{0007}' => out.write_str("\\a").map_err(|_| YamlWriteError::Fmt)?,
            '\u{0008}' => out.write_str("\\b").map_err(|_| YamlWriteError::Fmt)?,
            '\u{000B}' => out.write_str("\\v").map_err(|_| YamlWriteError::Fmt)?,
            '\u{000C}' => out.write_str("\\f").map_err(|_| YamlWriteError::Fmt)?,
            '\u{001B}' => out.write_str("\\e").map_err(|_| YamlWriteError::Fmt)?,
            '\u{0085}' => out.write_str("\\N").map_err(|_| YamlWriteError::Fmt)?,
            '\u{2028}' => out.write_str("\\L").map_err(|_| YamlWriteError::Fmt)?,
            '\u{2029}' => out.write_str("\\P").map_err(|_| YamlWriteError::Fmt)?,
            other => {
                let code = u32::from(other);
                if code < 0x20 || code == 0x7F || (0x80..=0x9F).contains(&code) {
                    write!(out, "\\u{code:04x}").map_err(|_| YamlWriteError::Fmt)?;
                } else {
                    out.write_char(other).map_err(|_| YamlWriteError::Fmt)?;
                }
            }
        }
    }
    out.write_char('"').map_err(|_| YamlWriteError::Fmt)
}

#[cfg(test)]
mod tests {
    use core::alloc::Allocator;

    use blaze_data::{Int, Map, Value};
    use blaze_own::{String as OwnString, Vec as OwnVec, format_in, testing::heap};
    use blaze_sim::{Broke, Config};

    use super::{
        YamlError, YamlLimits, YamlWhy, YamlWriteError, read_in, to_string_in,
        to_string_in_planted,
    };

    #[test]
    fn yaml_1_2_core_schema_and_norway_problem() {
        let h = heap();
        let alloc = h.alloc();
        let doc = concat!(
            "---\n",
            "# YAML 1.2 Core Schema\n",
            "country: NO\n",
            "agree: yes\n",
            "switch: on\n",
            "flag_t: true\n",
            "flag_f: FALSE\n",
            "empty_val:\n",
            "tilde_null: ~\n",
            "word_null: Null\n",
            "dec: -42\n",
            "hex: 0x2a\n",
            "oct: 0o52\n",
            "leading_zero: 010\n",
            "pi: 3.14159\n",
            "pos_inf: .inf\n",
            "neg_inf: -.Inf\n",
            "not_a_num: .NaN\n",
            "plain_inf_word: inf\n",
            "...\n",
        );

        let val = read_in(doc, YamlLimits::SHIPPED, alloc).expect("parse yaml");
        // Norway problem: NO / yes / on MUST remain Text in YAML 1.2 Core Schema!
        assert_eq!(val.get("country").and_then(Value::as_str), Some("NO"));
        assert_eq!(val.get("agree").and_then(Value::as_str), Some("yes"));
        assert_eq!(val.get("switch").and_then(Value::as_str), Some("on"));
        assert_eq!(val.get("plain_inf_word").and_then(Value::as_str), Some("inf"));

        assert_eq!(val.get("flag_t").and_then(Value::as_bool), Some(true));
        assert_eq!(val.get("flag_f").and_then(Value::as_bool), Some(false));
        assert!(val.get("empty_val").is_some_and(Value::is_null));
        assert!(val.get("tilde_null").is_some_and(Value::is_null));
        assert!(val.get("word_null").is_some_and(Value::is_null));

        assert_eq!(val.get("dec").and_then(Value::as_i64), Some(-42));
        assert_eq!(val.get("hex").and_then(Value::as_u64), Some(42));
        assert_eq!(val.get("oct").and_then(Value::as_u64), Some(42));
        assert_eq!(val.get("leading_zero").and_then(Value::as_u64), Some(10));

        assert_eq!(val.get("pi").and_then(Value::as_f64), Some(3.14159));
        assert_eq!(val.get("pos_inf").and_then(Value::as_f64), Some(f64::INFINITY));
        assert_eq!(val.get("neg_inf").and_then(Value::as_f64), Some(f64::NEG_INFINITY));
        assert!(val.get("not_a_num").and_then(Value::as_f64).is_some_and(f64::is_nan));
    }

    #[test]
    fn nested_block_flow_quotes_and_block_scalars() {
        let h = heap();
        let alloc = h.alloc();
        let doc = concat!(
            "services:\n",
            "  - name: api\n",
            "    ports: [80, 443, {internal: 8080, tls: true}]\n",
            "    quote1: 'it''s fine'\n",
            "    quote2: \"a\\nb\\t\\u0041\"\n",
            "  - name: worker\n",
            "    env:\n",
            "      MODE: prod\n",
            "items_same_indent:\n",
            "- first\n",
            "- second\n",
            "lit_clip: |\n",
            "  line 1\n",
            "  line 2\n",
            "lit_strip: |-\n",
            "  no trailing\n",
            "lit_keep: |+\n",
            "  keep two\n",
            "\n",
            "fold_clip: >\n",
            "  hello\n",
            "  world\n",
            "\n",
            "  para 2\n",
            "fold_strip: >-\n",
            "  folded\n",
            "  strip\n",
        );

        let val = read_in(doc, YamlLimits::SHIPPED, alloc).expect("parse complex yaml");
        let services = val.get("services").and_then(Value::as_list).expect("services list");
        assert_eq!(services.len(), 2);
        assert_eq!(services[0].get("name").and_then(Value::as_str), Some("api"));
        assert_eq!(services[0].get("quote1").and_then(Value::as_str), Some("it's fine"));
        assert_eq!(services[0].get("quote2").and_then(Value::as_str), Some("a\nb\tA"));

        let ports = services[0].get("ports").and_then(Value::as_list).expect("ports");
        assert_eq!(ports.len(), 3);
        assert_eq!(ports[0].as_u64(), Some(80));
        assert_eq!(ports[2].get("internal").and_then(Value::as_u64), Some(8080));
        assert_eq!(ports[2].get("tls").and_then(Value::as_bool), Some(true));

        let same = val
            .get("items_same_indent")
            .and_then(Value::as_list)
            .expect("same indent");
        assert_eq!(same.len(), 2);
        assert_eq!(same[0].as_str(), Some("first"));
        assert_eq!(same[1].as_str(), Some("second"));

        assert_eq!(val.get("lit_clip").and_then(Value::as_str), Some("line 1\nline 2\n"));
        assert_eq!(val.get("lit_strip").and_then(Value::as_str), Some("no trailing"));
        assert_eq!(val.get("lit_keep").and_then(Value::as_str), Some("keep two\n\n"));
        assert_eq!(val.get("fold_clip").and_then(Value::as_str), Some("hello world\npara 2\n"));
        assert_eq!(val.get("fold_strip").and_then(Value::as_str), Some("folded strip"));
    }

    #[test]
    fn duplicate_keys_limits_and_unsupported_bytes() {
        let h = heap();
        let alloc = h.alloc();

        assert!(matches!(
            read_in("a: 1\na: 2\n", YamlLimits::SHIPPED, alloc),
            Err(YamlError { line: 2, why: YamlWhy::DuplicateKey, .. })
        ));
        assert!(matches!(
            read_in("{a: 1, a: 2}", YamlLimits::SHIPPED, alloc),
            Err(YamlError { why: YamlWhy::DuplicateKey, .. })
        ));
        assert!(matches!(
            read_in("[[[1]]]", YamlLimits { most_bytes: 1024, most_depth: 2, most_nodes: 100 }, alloc),
            Err(YamlError { why: YamlWhy::TooDeep { limit: 2 }, .. })
        ));
        assert!(matches!(
            read_in("[1, 2, 3, 4]", YamlLimits { most_bytes: 1024, most_depth: 16, most_nodes: 3 }, alloc),
            Err(YamlError { why: YamlWhy::TooManyNodes { limit: 3 }, .. })
        ));
        assert!(matches!(
            read_in("\ta: 1\n", YamlLimits::SHIPPED, alloc),
            Err(YamlError { why: YamlWhy::TabIndentation, .. })
        ));

        let mut bytes = OwnVec::new_in(alloc);
        bytes.push(0xDE);
        assert_eq!(to_string_in(&Value::Bytes(bytes), alloc), Err(YamlWriteError::UnsupportedBytes));
    }

    fn draw_value<A: Allocator + Copy>(
        tape: &blaze_sim::Tape,
        max_depth: usize,
        alloc: A,
    ) -> Value<A> {
        const TEXTS: &[&str] = &[
            "",
            "hello",
            "NO",
            "yes",
            "true",
            "false",
            "null",
            "~",
            "123",
            "-42",
            "0x10",
            "0o77",
            "3.14",
            ".inf",
            "-.inf",
            ".nan",
            " leading",
            "trailing ",
            "a: b",
            "line1\nline2",
            "quote\"and'and\\slash",
            "café 🚀",
            "- dash",
            "[flow]",
        ];
        const KEYS: &[&str] = &["a", "b", "c", "true", "NO", "123", "with space", "k:v", ""];

        // Iterative generator (matches no-recursion discipline in tests too).
        enum GenSlot<A: Allocator> {
            List { items: OwnVec<Value<A>, A>, remaining: usize, depth: usize },
            Map { map: Map<A>, remaining: usize, depth: usize, next_key: usize },
        }

        let draw_leaf = |tape: &blaze_sim::Tape| -> Value<A> {
            match tape.int_in(0u8..=4, "leaf") {
                0 => Value::Null,
                1 => Value::Bool(tape.coin("bool")),
                2 => {
                    if tape.coin("neg") {
                        Value::Int(Int::from_i64(tape.int_in(-10_000i64..=-1, "i64")))
                    } else {
                        Value::Int(Int::from_u64(tape.int_in(0u64..=100_000, "u64")))
                    }
                }
                3 => {
                    const FLOATS: &[f64] = &[0.0, -0.0, 1.5, -2.25, 1e20, f64::INFINITY, f64::NEG_INFINITY, f64::NAN];
                    Value::Float(FLOATS[tape.index(FLOATS.len(), "float")])
                }
                _ => Value::Text(OwnString::from_str_in(
                    TEXTS[tape.index(TEXTS.len(), "text")],
                    alloc,
                )),
            }
        };

        let mut stack: OwnVec<GenSlot<A>, A> = OwnVec::new_in(alloc);
        let mut current_depth = 0usize;
        let mut root: Option<Value<A>> = None;

        loop {
            let can_nest = current_depth < max_depth;
            let pick = if can_nest {
                tape.int_in(0u8..=6, "node_kind")
            } else {
                0
            };

            let produced = match pick {
                5 => {
                    let count = tape.int_in(0usize..=4, "list_len");
                    if count == 0 {
                        Some(Value::List(OwnVec::new_in(alloc)))
                    } else {
                        current_depth += 1;
                        stack.push(GenSlot::List {
                            items: OwnVec::with_capacity_in(count, alloc),
                            remaining: count,
                            depth: current_depth,
                        });
                        None
                    }
                }
                6 => {
                    let count = tape.int_in(0usize..=4, "map_len");
                    if count == 0 {
                        Some(Value::Map(Map::new_in(alloc)))
                    } else {
                        current_depth += 1;
                        stack.push(GenSlot::Map {
                            map: Map::with_capacity_in(count, alloc),
                            remaining: count,
                            depth: current_depth,
                            next_key: 0,
                        });
                        None
                    }
                }
                _ => Some(draw_leaf(tape)),
            };

            let Some(mut val) = produced else {
                continue;
            };

            loop {
                match stack.last_mut() {
                    None => {
                        root = Some(val);
                        break;
                    }
                    Some(GenSlot::List { items, remaining, depth }) => {
                        items.push(val);
                        *remaining -= 1;
                        if *remaining > 0 {
                            current_depth = *depth;
                            break;
                        }
                        if let Some(GenSlot::List { items, depth, .. }) = stack.pop() {
                            current_depth = depth - 1;
                            val = Value::List(items);
                        } else {
                            break;
                        }
                    }
                    Some(GenSlot::Map { map, remaining, depth, next_key }) => {
                        let base = KEYS[*next_key % KEYS.len()];
                        let key = format_in!(alloc, "{base}{}", *next_key);
                        *next_key += 1;
                        map.insert(key, val);
                        *remaining -= 1;
                        if *remaining > 0 {
                            current_depth = *depth;
                            break;
                        }
                        if let Some(GenSlot::Map { map, depth, .. }) = stack.pop() {
                            current_depth = depth - 1;
                            val = Value::Map(map);
                        } else {
                            break;
                        }
                    }
                }
            }

            if root.is_some() {
                break;
            }
        }

        root.unwrap_or(Value::Null)
    }

    fn check_yaml_round_trip<A: Allocator + Copy>(
        val: &Value<A>,
        plant_skip_keyword_quote: bool,
        alloc: A,
    ) -> Result<(), Broke> {
        let text = to_string_in_planted(val, alloc, plant_skip_keyword_quote)
            .map_err(|e| Broke::Oracle(format_in!(alloc, "write failed: {e}").as_str().into()))?;
        let parsed = read_in(text.as_str(), YamlLimits::SHIPPED, alloc).map_err(|e| {
            Broke::Oracle(
                format_in!(alloc, "read failed: {e}\nYAML:\n{}", text.as_str())
                    .as_str()
                    .into(),
            )
        })?;
        if &parsed != val {
            return Err(Broke::Oracle(
                format_in!(
                    alloc,
                    "round-trip mismatch:\nexpected: {val:?}\ngot:      {parsed:?}\nYAML:\n{}",
                    text.as_str()
                )
                .as_str()
                .into(),
            ));
        }
        Ok(())
    }

    /// The invariant: YAML serialization and parsing round-trip structured values including quoted keyword strings.
    #[test]
    fn property_yaml_round_trip() {
        let h = heap();
        blaze_sim::check(Config::cases(256), |tape| -> Result<(), Broke> {
            let val = draw_value(tape, 3, h.alloc());
            check_yaml_round_trip(&val, false, h.alloc())
        });
    }

    #[test]
    fn the_oracle_catches_unquoted_keyword_strings() {
        let h = heap();
        blaze_sim::control(Config::cases(128), false, true, "", |tape, plant| {
            let val = draw_value(tape, 2, h.alloc());
            check_yaml_round_trip(&val, plant, h.alloc())
        });
    }

    #[test]
    fn hostile_inputs_never_panic_or_hang() {
        let h = heap();
        let alloc = h.alloc();
        const TOKENS: &[&str] = &[
            "- ", ": ", "  ", "\n", "\r\n", "[", "]", "{", "}", ",", "\"", "'", "\\", "|", ">",
            "|-", ">+", "---\n", "...\n", "# c\n", "a", "1", "true", "null", "\t", "''",
        ];
        blaze_sim::check(Config::cases(256), |tape| -> Result<(), Broke> {
            let len = tape.int_in(0usize..=24, "hostile_len");
            let mut doc = OwnString::new_in(alloc);
            for _ in 0..len {
                doc.push_str(TOKENS[tape.index(TOKENS.len(), "tok")]);
            }
            let limits = YamlLimits { most_bytes: 4096, most_depth: 16, most_nodes: 256 };
            drop(read_in(doc.as_str(), limits, alloc));
            Ok(())
        });
    }
}
