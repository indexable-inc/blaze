//! Zero-recursion iterative parser for `blaze-tab`.
//!
//! Reads UTF-8 bytes into a [`blaze_data::Value`] using [`blaze_data::Build`] and an explicit
//! allocator-backed stack of open containers (`Vec<Frame, Alloc<'a>>`). No function recurses,
//! no unchecked index or stop macro is used, and every error reports byte offset, `line:col`,
//! and structural path.

use core::fmt::Write as _;

use blaze_data::{Build, Int, Step, Value, write_step};
use blaze_own::{Alloc, BTreeSet, String, Vec};

use crate::{
    Duplicates, Expected, Limits, ReadError, What, is_bare_key_cont, is_bare_start, is_bare_value_cont,
    is_reserved_word, line_col,
};

/// Open container frame on the parser's explicit stack.
enum Frame<'a> {
    List {
        next_index: usize,
        need_sep: bool,
    },
    Map {
        active_key: Option<String<Alloc<'a>>>,
        need_sep: bool,
    },
    Table {
        cols: Vec<String<Alloc<'a>>, Alloc<'a>>,
        row_index: usize,
        need_sep: bool,
    },
    TableRow {
        col_index: usize,
        need_sep: bool,
    },
}

/// Result of checking separator / closing delimiter inside a container.
enum SepResult {
    Close,
    Continue,
}

struct Parser<'t, 'a> {
    text: &'t str,
    at: usize,
    limits: Limits,
    alloc: Alloc<'a>,
    frames: Vec<Frame<'a>, Alloc<'a>>,
    build: Build<Alloc<'a>, Alloc<'a>>,
    done: bool,
}

/// Parses `bytes` as one `blaze-tab` [`Value`] in `alloc` under `limits`.
///
/// Enforces UTF-8, byte-length, nesting-depth, and number-length limits, rejects trailing tokens,
/// duplicate keys (under [`Duplicates::Refuse`]), duplicate table columns, and table row arity
/// mismatches.
pub fn read<'a>(bytes: &[u8], limits: Limits, alloc: Alloc<'a>) -> Result<Value<Alloc<'a>>, ReadError<Alloc<'a>>> {
    if bytes.len() > limits.most_bytes {
        return Err(ReadError {
            at: 0,
            line: 1,
            col: 1,
            path: String::new_in(alloc),
            what: What::TooLarge { len: bytes.len(), most: limits.most_bytes },
        });
    }
    let text = match core::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => {
            let at = error.valid_up_to();
            let (line, col) = line_col(bytes, at);
            return Err(ReadError { at, line, col, path: String::new_in(alloc), what: What::Utf8 });
        }
    };

    let parser = Parser {
        text,
        at: 0,
        limits,
        alloc,
        frames: Vec::new_in(alloc),
        build: Build::new_in(alloc, alloc),
        done: false,
    };

    parser.parse_document()
}

impl<'t, 'a> Parser<'t, 'a> {
    fn bytes(&self) -> &'t [u8] {
        self.text.as_bytes()
    }

    fn peek(&self) -> Option<u8> {
        self.bytes().get(self.at).copied()
    }

    fn peek_at(&self, pos: usize) -> Option<u8> {
        self.bytes().get(pos).copied()
    }

    fn current_path(&self, extra_key: Option<&str>) -> String<Alloc<'a>> {
        let mut out = String::new_in(self.alloc);
        let mut before = false;
        let len = self.frames.len();
        let mut i = 0;
        while i < len {
            match self.frames.get(i) {
                Some(Frame::List { next_index, .. }) => {
                    if let Ok(next_before) = write_step(&mut out, Step::Index(*next_index), before) {
                        before = next_before;
                    }
                }
                Some(Frame::Map { active_key: Some(key), .. }) => {
                    if let Ok(next_before) = write_step(&mut out, Step::Key(key.as_str()), before) {
                        before = next_before;
                    }
                }
                Some(Frame::Map { active_key: None, .. }) => {}
                Some(Frame::Table { cols, row_index, .. }) => {
                    if let Some(Frame::TableRow { col_index, .. }) = self.frames.get(i + 1) {
                        if let Ok(next_before) = write_step(&mut out, Step::Index(*row_index), before) {
                            before = next_before;
                        }
                        if let Some(col_name) = cols.get(*col_index)
                            && let Ok(next_before) = write_step(&mut out, Step::Key(col_name.as_str()), before)
                        {
                            before = next_before;
                        }
                        i += 1;
                    }
                }
                Some(Frame::TableRow { .. }) | None => {}
            }
            i += 1;
        }
        if let Some(key) = extra_key
            && write_step(&mut out, Step::Key(key), before).is_err()
        {
            return out;
        }
        out
    }

    fn err(&self, at: usize, what: What) -> ReadError<Alloc<'a>> {
        let (line, col) = line_col(self.bytes(), at);
        ReadError { at, line, col, path: self.current_path(None), what }
    }

    fn err_with_key(&self, at: usize, key: &str, what: What) -> ReadError<Alloc<'a>> {
        let (line, col) = line_col(self.bytes(), at);
        ReadError { at, line, col, path: self.current_path(Some(key)), what }
    }

    fn unexpected(&self, at: usize, expected: Expected) -> ReadError<Alloc<'a>> {
        match self.text.get(at..).and_then(|s| s.chars().next()) {
            None => self.err(at, What::End { expected }),
            Some(found) => self.err(at, What::Unexpected { found, expected }),
        }
    }

    fn skip_ws_and_comments_from(&self, mut pos: usize) -> usize {
        let bytes = self.bytes();
        loop {
            while let Some(b' ' | b'\t' | b'\n' | b'\r') = bytes.get(pos).copied() {
                pos += 1;
            }
            if bytes.get(pos) == Some(&b'#') {
                pos += 1;
                while let Some(&b) = bytes.get(pos) {
                    pos += 1;
                    if b == b'\n' {
                        break;
                    }
                }
            } else {
                break;
            }
        }
        pos
    }

    fn skip_ws_and_comments(&mut self) {
        self.at = self.skip_ws_and_comments_from(self.at);
    }

    /// Consumes whitespace/comments, an optional comma if `need_sep`, and detects closing byte `close`.
    fn container_sep(&mut self, close: u8, need_sep: bool, sep_expected: Expected) -> Result<SepResult, ReadError<Alloc<'a>>> {
        let before = self.at;
        self.skip_ws_and_comments();
        if self.peek() == Some(close) {
            return Ok(SepResult::Close);
        }
        if !need_sep {
            return Ok(SepResult::Continue);
        }
        let had_ws = self.at > before;
        if self.peek() == Some(b',') {
            self.at += 1;
            self.skip_ws_and_comments();
            if self.peek() == Some(close) {
                return Ok(SepResult::Close);
            }
            return Ok(SepResult::Continue);
        }
        if had_ws {
            return Ok(SepResult::Continue);
        }
        Err(self.unexpected(self.at, sep_expected))
    }

    /// Updates the enclosing frame after a complete value (scalar or closed container) was produced.
    fn finish_value(&mut self) {
        match self.frames.last_mut() {
            None => self.done = true,
            Some(Frame::List { next_index, need_sep }) => {
                *next_index += 1;
                *need_sep = true;
            }
            Some(Frame::Map { active_key, need_sep }) => {
                *active_key = None;
                *need_sep = true;
            }
            Some(Frame::TableRow { col_index, need_sep }) => {
                *col_index += 1;
                *need_sep = true;
            }
            Some(Frame::Table { .. }) => {}
        }
    }

    fn parse_document(mut self) -> Result<Value<Alloc<'a>>, ReadError<Alloc<'a>>> {
        self.skip_ws_and_comments();
        self.parse_value(Expected::Value)?;

        while !self.done {
            match self.frames.last() {
                None => {
                    self.done = true;
                }
                Some(Frame::List { need_sep, .. }) => {
                    let need_sep = *need_sep;
                    match self.container_sep(b']', need_sep, Expected::SeparatorOrEndList)? {
                        SepResult::Close => {
                            self.at += 1;
                            self.build.end();
                            let _ = self.frames.pop();
                            self.finish_value();
                        }
                        SepResult::Continue => {
                            let expected = match need_sep {
                                true => Expected::Value,
                                false => Expected::ValueOrEndList,
                            };
                            self.parse_value(expected)?;
                        }
                    }
                }
                Some(Frame::Map { need_sep, .. }) => {
                    let need_sep = *need_sep;
                    match self.container_sep(b'}', need_sep, Expected::SeparatorOrEndMap)? {
                        SepResult::Close => {
                            self.at += 1;
                            self.build.end();
                            let _ = self.frames.pop();
                            self.finish_value();
                        }
                        SepResult::Continue => {
                            let expected = match need_sep {
                                true => Expected::Key,
                                false => Expected::KeyOrEndMap,
                            };
                            let key_at = self.at;
                            let key = self.parse_key(expected)?;
                            self.skip_ws_and_comments();
                            if self.peek() != Some(b':') {
                                return Err(self.unexpected(self.at, Expected::Colon));
                            }
                            self.at += 1;
                            match self.limits.duplicates {
                                Duplicates::Refuse => {
                                    let key_copy = String::from_str_in(key.as_str(), self.alloc);
                                    if self.build.key(key_copy).is_err() {
                                        return Err(self.err_with_key(key_at, key.as_str(), What::DuplicateKey));
                                    }
                                }
                                Duplicates::LastWins => {
                                    let key_copy = String::from_str_in(key.as_str(), self.alloc);
                                    self.build.key_last_wins(key_copy);
                                }
                            }
                            if let Some(Frame::Map { active_key, .. }) = self.frames.last_mut() {
                                *active_key = Some(key);
                            }
                            self.skip_ws_and_comments();
                            self.parse_value(Expected::Value)?;
                        }
                    }
                }
                Some(Frame::Table { cols, need_sep, .. }) => {
                    let need_sep = *need_sep;
                    let col_count = cols.len();
                    match self.container_sep(b']', need_sep, Expected::SeparatorOrEndTable)? {
                        SepResult::Close => {
                            self.at += 1;
                            self.build.end();
                            let _ = self.frames.pop();
                            self.finish_value();
                        }
                        SepResult::Continue => {
                            if self.peek() != Some(b'[') {
                                return Err(self.unexpected(self.at, Expected::RowOrEndTable));
                            }
                            if self.frames.len() >= self.limits.most_depth {
                                return Err(self.err(self.at, What::TooDeep { most: self.limits.most_depth }));
                            }
                            self.at += 1;
                            self.build.start_map(col_count);
                            self.frames.push(Frame::TableRow { col_index: 0, need_sep: false });
                        }
                    }
                }
                Some(Frame::TableRow { col_index, need_sep }) => {
                    let col_index = *col_index;
                    let need_sep = *need_sep;
                    let (row_index, col_count) = self.enclosing_table_info();
                    match self.container_sep(b']', need_sep, Expected::SeparatorOrEndRow)? {
                        SepResult::Close => {
                            if col_index < col_count {
                                return Err(self.err(
                                    self.at,
                                    What::RowArity { row: row_index, expected: col_count, found: col_index },
                                ));
                            }
                            self.at += 1;
                            self.build.end();
                            let _ = self.frames.pop();
                            if let Some(Frame::Table { row_index, need_sep, .. }) = self.frames.last_mut() {
                                *row_index += 1;
                                *need_sep = true;
                            }
                        }
                        SepResult::Continue => {
                            if col_index >= col_count {
                                return Err(self.err(
                                    self.at,
                                    What::RowArity { row: row_index, expected: col_count, found: col_index + 1 },
                                ));
                            }
                            if self.try_consume_absent() {
                                if let Some(Frame::TableRow { col_index, need_sep }) = self.frames.last_mut() {
                                    *col_index += 1;
                                    *need_sep = true;
                                }
                            } else {
                                if let Some(col_name) = self.enclosing_table_col(col_index) {
                                    self.build.key_last_wins(col_name);
                                }
                                self.parse_value(Expected::CellOrEndRow)?;
                            }
                        }
                    }
                }
            }
        }

        self.skip_ws_and_comments();
        if self.peek().is_some() {
            return Err(self.unexpected(self.at, Expected::End));
        }

        let at = self.at;
        let bytes = self.bytes();
        let alloc = self.alloc;
        match self.build.finish() {
            Some(value) => Ok(value),
            None => {
                let (line, col) = line_col(bytes, at);
                Err(ReadError { at, line, col, path: String::new_in(alloc), what: What::End { expected: Expected::Value } })
            }
        }
    }

    fn enclosing_table_info(&self) -> (usize, usize) {
        let idx = self.frames.len().saturating_sub(2);
        match self.frames.get(idx) {
            Some(Frame::Table { cols, row_index, .. }) => (*row_index, cols.len()),
            _ => (0, 0),
        }
    }

    fn enclosing_table_col(&self, col_index: usize) -> Option<String<Alloc<'a>>> {
        let idx = self.frames.len().saturating_sub(2);
        match self.frames.get(idx) {
            Some(Frame::Table { cols, .. }) => {
                cols.get(col_index).map(|s| String::from_str_in(s.as_str(), self.alloc))
            }
            _ => None,
        }
    }

    /// Returns `true` and advances past `_` if the cursor stands at the exact absent sentinel `_`.
    fn try_consume_absent(&mut self) -> bool {
        if self.peek() != Some(b'_') {
            return false;
        }
        let next = self.peek_at(self.at + 1);
        if next.is_some_and(is_bare_value_cont) {
            return false;
        }
        self.at += 1;
        true
    }

    /// Parses one value at the cursor: either pushes a scalar to `build` and calls `finish_value`,
    /// or opens a container (`List`, `Map`, or `Table`) onto `frames`.
    fn parse_value(&mut self, expected: Expected) -> Result<(), ReadError<Alloc<'a>>> {
        let at = self.at;
        match self.peek() {
            Some(b'{') => {
                if self.frames.len() >= self.limits.most_depth {
                    return Err(self.err(at, What::TooDeep { most: self.limits.most_depth }));
                }
                self.at += 1;
                self.build.start_map(0);
                self.frames.push(Frame::Map { active_key: None, need_sep: false });
                Ok(())
            }
            Some(b'[') => {
                let inner_pos = self.skip_ws_and_comments_from(at + 1);
                if self.peek_at(inner_pos) == Some(b'[') && self.is_table_header_at(inner_pos) {
                    self.open_table(at, inner_pos)
                } else {
                    if self.frames.len() >= self.limits.most_depth {
                        return Err(self.err(at, What::TooDeep { most: self.limits.most_depth }));
                    }
                    self.at += 1;
                    self.build.start_list(0);
                    self.frames.push(Frame::List { next_index: 0, need_sep: false });
                    Ok(())
                }
            }
            Some(b'"') => {
                let s = self.parse_string()?;
                self.build.value(Value::Text(s));
                self.finish_value();
                Ok(())
            }
            Some(b'0') if self.bytes().get(at..at + 3) == Some(b"0x[") => {
                let bytes = self.parse_hex_bytes()?;
                self.build.value(Value::Bytes(bytes));
                self.finish_value();
                Ok(())
            }
            Some(b'-') => {
                if self.bytes().get(at..at + 4) == Some(b"-inf")
                    && !self.peek_at(at + 4).is_some_and(is_bare_value_cont)
                {
                    self.at += 4;
                    self.build.value(Value::Float(f64::NEG_INFINITY));
                    self.finish_value();
                    Ok(())
                } else {
                    let val = self.parse_number()?;
                    self.build.value(val);
                    self.finish_value();
                    Ok(())
                }
            }
            Some(b'0'..=b'9') => {
                let val = self.parse_number()?;
                self.build.value(val);
                self.finish_value();
                Ok(())
            }
            Some(b) if is_bare_start(b) => {
                let word = self.scan_bare_value();
                match word {
                    "null" => self.build.value(Value::Null),
                    "true" => self.build.value(Value::Bool(true)),
                    "false" => self.build.value(Value::Bool(false)),
                    "inf" => self.build.value(Value::Float(f64::INFINITY)),
                    "nan" => self.build.value(Value::Float(f64::NAN)),
                    "_" => return Err(self.err(at, What::AbsentOutsideTable)),
                    text => self.build.value(Value::Text(String::from_str_in(text, self.alloc))),
                }
                self.finish_value();
                Ok(())
            }
            _ => Err(self.unexpected(at, expected)),
        }
    }

    /// Bounded lookahead: checks whether the `[` at `inner_lbracket` is a flat list of column
    /// names followed by `];`. Every byte of the document is visited at most once across all
    /// calls because any nested `[` stops immediately on byte 1.
    fn is_table_header_at(&self, inner_lbracket: usize) -> bool {
        let bytes = self.bytes();
        let mut pos = inner_lbracket + 1;
        let mut need_sep = false;
        loop {
            let before = pos;
            pos = self.skip_ws_and_comments_from(pos);
            if bytes.get(pos) == Some(&b']') {
                let after = self.skip_ws_and_comments_from(pos + 1);
                return bytes.get(after) == Some(&b';');
            }
            if need_sep {
                let had_ws = pos > before;
                if bytes.get(pos) == Some(&b',') {
                    pos = self.skip_ws_and_comments_from(pos + 1);
                    if bytes.get(pos) == Some(&b']') {
                        let after = self.skip_ws_and_comments_from(pos + 1);
                        return bytes.get(after) == Some(&b';');
                    }
                } else if !had_ws {
                    return false;
                }
            }
            match bytes.get(pos).copied() {
                Some(b'"') => {
                    let Some(next_pos) = self.skip_quoted_lookahead(pos) else {
                        return false;
                    };
                    pos = next_pos;
                }
                Some(b) if is_bare_start(b) => {
                    pos += 1;
                    while bytes.get(pos).copied().is_some_and(is_bare_key_cont) {
                        pos += 1;
                    }
                }
                _ => return false,
            }
            need_sep = true;
        }
    }

    fn skip_quoted_lookahead(&self, start: usize) -> Option<usize> {
        let bytes = self.bytes();
        if bytes.get(start..start + 3) == Some(b"\"\"\"") {
            let mut i = start + 3;
            while let Some(&b) = bytes.get(i) {
                if b == b'\\' {
                    i += 2;
                    continue;
                }
                if bytes.get(i..i + 3) == Some(b"\"\"\"") {
                    let mut end = i + 3;
                    while bytes.get(end) == Some(&b'"') {
                        end += 1;
                    }
                    return Some(end);
                }
                i += 1;
            }
            None
        } else {
            let mut i = start + 1;
            while let Some(&b) = bytes.get(i) {
                match b {
                    b'"' => return Some(i + 1),
                    b'\\' => i += 2,
                    _ => i += 1,
                }
            }
            None
        }
    }

    fn open_table(&mut self, outer_at: usize, inner_lbracket: usize) -> Result<(), ReadError<Alloc<'a>>> {
        if self.frames.len() >= self.limits.most_depth {
            return Err(self.err(outer_at, What::TooDeep { most: self.limits.most_depth }));
        }
        self.at = inner_lbracket + 1;
        let mut cols = Vec::new_in(self.alloc);
        let mut seen = BTreeSet::new_in(self.alloc);
        let mut need_sep = false;

        loop {
            match self.container_sep(b']', need_sep, Expected::SeparatorOrEndHeader)? {
                SepResult::Close => {
                    self.at += 1;
                    break;
                }
                SepResult::Continue => {
                    let col_at = self.at;
                    let col = self.parse_key(Expected::ColumnName)?;
                    let col_check = String::from_str_in(col.as_str(), self.alloc);
                    if !seen.insert(col_check) {
                        return Err(self.err_with_key(col_at, col.as_str(), What::DuplicateColumn));
                    }
                    cols.push(col);
                    need_sep = true;
                }
            }
        }

        if cols.is_empty() {
            return Err(self.err(inner_lbracket, What::EmptyTableHeader));
        }

        self.skip_ws_and_comments();
        if self.peek() != Some(b';') {
            return Err(self.unexpected(self.at, Expected::Semicolon));
        }
        self.at += 1;

        self.build.start_list(0);
        self.frames.push(Frame::Table { cols, row_index: 0, need_sep: false });
        Ok(())
    }

    fn parse_key(&mut self, expected: Expected) -> Result<String<Alloc<'a>>, ReadError<Alloc<'a>>> {
        let at = self.at;
        match self.peek() {
            Some(b'"') => self.parse_string(),
            Some(b) if is_bare_start(b) => {
                let mut end = at + 1;
                while self.peek_at(end).is_some_and(is_bare_key_cont) {
                    end += 1;
                }
                let word = self.text.get(at..end).unwrap_or("");
                if is_reserved_word(word) {
                    return Err(self.err(at, What::ReservedKey));
                }
                self.at = end;
                Ok(String::from_str_in(word, self.alloc))
            }
            _ => Err(self.unexpected(at, expected)),
        }
    }

    fn scan_bare_value(&mut self) -> &'t str {
        let start = self.at;
        let mut end = start + 1;
        while self.peek_at(end).is_some_and(is_bare_value_cont) {
            end += 1;
        }
        self.at = end;
        self.text.get(start..end).unwrap_or("")
    }

    /// Parses `0x[...]` hex byte literal; optional spaces, tabs, newlines, and `_` allowed inside.
    fn parse_hex_bytes(&mut self) -> Result<Vec<u8, Alloc<'a>>, ReadError<Alloc<'a>>> {
        let start = self.at;
        self.at += 3; // consume `0x[`
        let mut out = Vec::new_in(self.alloc);
        let mut high: Option<u8> = None;

        loop {
            match self.peek() {
                None => return Err(self.err(self.at, What::End { expected: Expected::EndBytes })),
                Some(b']') => {
                    if high.is_some() {
                        return Err(self.err(start, What::OddHexBytes));
                    }
                    self.at += 1;
                    return Ok(out);
                }
                Some(b' ' | b'\t' | b'\n' | b'\r' | b'_') => {
                    self.at += 1;
                }
                Some(b) => match hex_nibble(b) {
                    Some(nibble) => {
                        self.at += 1;
                        match high.take() {
                            None => high = Some(nibble),
                            Some(hi) => out.push((hi << 4) | nibble),
                        }
                    }
                    None => return Err(self.unexpected(self.at, Expected::HexDigit)),
                },
            }
        }
    }

    /// Parses a double-quoted `"..."` or triple-quoted `"""..."""` string.
    fn parse_string(&mut self) -> Result<String<Alloc<'a>>, ReadError<Alloc<'a>>> {
        if self.bytes().get(self.at..self.at + 3) == Some(b"\"\"\"") {
            self.parse_multiline_string()
        } else {
            self.parse_basic_string()
        }
    }

    fn parse_basic_string(&mut self) -> Result<String<Alloc<'a>>, ReadError<Alloc<'a>>> {
        let bytes = self.bytes();
        let mut i = self.at + 1;
        let mut out = String::new_in(self.alloc);
        let mut run = i;

        loop {
            match bytes.get(i).copied() {
                None => return Err(self.err(i, What::End { expected: Expected::Quote })),
                Some(b'"') => {
                    if let Some(chunk) = self.text.get(run..i) {
                        out.push_str(chunk);
                    }
                    self.at = i + 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    if let Some(chunk) = self.text.get(run..i) {
                        out.push_str(chunk);
                    }
                    i = self.parse_escape(i, &mut out, Expected::Quote)?;
                    run = i;
                }
                Some(byte) if byte < 0x20 => return Err(self.err(i, What::Control { byte })),
                Some(_) => i += 1,
            }
        }
    }

    fn parse_multiline_string(&mut self) -> Result<String<Alloc<'a>>, ReadError<Alloc<'a>>> {
        let bytes = self.bytes();
        let mut i = self.at + 3;
        // Trim one immediate leading newline after `"""`.
        if bytes.get(i) == Some(&b'\r') && bytes.get(i + 1) == Some(&b'\n') {
            i += 2;
        } else if bytes.get(i) == Some(&b'\n') {
            i += 1;
        }

        let mut out = String::new_in(self.alloc);
        let mut run = i;

        loop {
            match bytes.get(i).copied() {
                None => return Err(self.err(i, What::End { expected: Expected::TripleQuote })),
                Some(b'"') if bytes.get(i..i + 3) == Some(b"\"\"\"") => {
                    if let Some(chunk) = self.text.get(run..i) {
                        out.push_str(chunk);
                    }
                    // Count total consecutive quotes: 3 closes; 4 or 5 put 1 or 2 quotes in content.
                    let mut quotes = 3usize;
                    while bytes.get(i + quotes) == Some(&b'"') && quotes < 5 {
                        quotes += 1;
                    }
                    let extra = quotes - 3;
                    for _ in 0..extra {
                        out.push('"');
                    }
                    self.at = i + quotes;
                    return Ok(out);
                }
                Some(b'\\') => {
                    if let Some(chunk) = self.text.get(run..i) {
                        out.push_str(chunk);
                    }
                    i = self.parse_escape(i, &mut out, Expected::TripleQuote)?;
                    run = i;
                }
                Some(byte) if byte < 0x20 && !matches!(byte, b'\n' | b'\r' | b'\t') => {
                    return Err(self.err(i, What::Control { byte }));
                }
                Some(_) => i += 1,
            }
        }
    }

    /// Decodes one escape sequence whose `\` is at `i`, appending to `out`, returning the index past it.
    fn parse_escape(
        &self,
        i: usize,
        out: &mut String<Alloc<'a>>,
        end_expected: Expected,
    ) -> Result<usize, ReadError<Alloc<'a>>> {
        let bytes = self.bytes();
        match bytes.get(i + 1).copied() {
            None => Err(self.err(i + 1, What::End { expected: end_expected })),
            Some(b'"') => {
                out.push('"');
                Ok(i + 2)
            }
            Some(b'\\') => {
                out.push('\\');
                Ok(i + 2)
            }
            Some(b'/') => {
                out.push('/');
                Ok(i + 2)
            }
            Some(b'b') => {
                out.push('\u{08}');
                Ok(i + 2)
            }
            Some(b'f') => {
                out.push('\u{0C}');
                Ok(i + 2)
            }
            Some(b'n') => {
                out.push('\n');
                Ok(i + 2)
            }
            Some(b'r') => {
                out.push('\r');
                Ok(i + 2)
            }
            Some(b't') => {
                out.push('\t');
                Ok(i + 2)
            }
            Some(b'0') => {
                out.push('\0');
                Ok(i + 2)
            }
            // Line-continuation escape: `\` followed by optional spaces/tabs then newline skips leading ws.
            Some(b'\n') => {
                let mut next = i + 2;
                while matches!(bytes.get(next), Some(b' ' | b'\t' | b'\n' | b'\r')) {
                    next += 1;
                }
                Ok(next)
            }
            Some(b'\r') if bytes.get(i + 2) == Some(&b'\n') => {
                let mut next = i + 3;
                while matches!(bytes.get(next), Some(b' ' | b'\t' | b'\n' | b'\r')) {
                    next += 1;
                }
                Ok(next)
            }
            Some(b'u') => {
                if bytes.get(i + 2) == Some(&b'{') {
                    self.parse_braced_unicode_escape(i, out)
                } else {
                    self.parse_json_unicode_escape(i, out)
                }
            }
            Some(_) => {
                let found = self.text.get(i + 1..).and_then(|s| s.chars().next()).unwrap_or('\u{FFFD}');
                Err(self.err(i + 1, What::BadEscape { found }))
            }
        }
    }

    fn parse_braced_unicode_escape(
        &self,
        backslash_at: usize,
        out: &mut String<Alloc<'a>>,
    ) -> Result<usize, ReadError<Alloc<'a>>> {
        let bytes = self.bytes();
        let mut pos = backslash_at + 3; // after `\u{`
        let mut value: u32 = 0;
        let mut digits: usize = 0;

        while let Some(&b) = bytes.get(pos) {
            if b == b'}' {
                if digits == 0 {
                    return Err(self.err(backslash_at, What::InvalidUnicodeEscape));
                }
                let Some(ch) = char::from_u32(value) else {
                    return Err(self.err(backslash_at, What::InvalidUnicodeEscape));
                };
                out.push(ch);
                return Ok(pos + 1);
            }
            let Some(nibble) = hex_nibble(b) else {
                return Err(self.unexpected(pos, Expected::HexDigit));
            };
            digits += 1;
            if digits > 6 {
                return Err(self.err(backslash_at, What::InvalidUnicodeEscape));
            }
            value = (value << 4) | u32::from(nibble);
            pos += 1;
        }
        Err(self.err(pos, What::End { expected: Expected::HexEscapeBrace }))
    }

    fn parse_json_unicode_escape(
        &self,
        backslash_at: usize,
        out: &mut String<Alloc<'a>>,
    ) -> Result<usize, ReadError<Alloc<'a>>> {
        let bytes = self.bytes();
        let Some(unit) = hex4(bytes, backslash_at + 2) else {
            return Err(self.unexpected(backslash_at + 2, Expected::HexDigit));
        };
        match unit {
            0xD800..=0xDBFF => {
                let low = match bytes.get(backslash_at + 6..backslash_at + 8) {
                    Some(b"\\u") => hex4(bytes, backslash_at + 8),
                    _ => None,
                };
                match low {
                    Some(lo @ 0xDC00..=0xDFFF) => {
                        let cp = 0x10000 + ((unit - 0xD800) << 10) + (lo - 0xDC00);
                        let Some(ch) = char::from_u32(cp) else {
                            return Err(self.err(backslash_at, What::InvalidUnicodeEscape));
                        };
                        out.push(ch);
                        Ok(backslash_at + 12)
                    }
                    _ => Err(self.err(backslash_at, What::LoneSurrogate)),
                }
            }
            0xDC00..=0xDFFF => Err(self.err(backslash_at, What::LoneSurrogate)),
            _ => {
                let Some(ch) = char::from_u32(unit) else {
                    return Err(self.err(backslash_at, What::InvalidUnicodeEscape));
                };
                out.push(ch);
                Ok(backslash_at + 6)
            }
        }
    }

    /// Scans a digit run starting at `start` (`[0-9]([0-9_]*[0-9])?`, disallowing trailing/adjacent `_`).
    /// Returns `(end_offset, has_underscore)`.
    fn scan_digit_run(&self, start: usize) -> Result<(usize, bool), ReadError<Alloc<'a>>> {
        let bytes = self.bytes();
        if !bytes.get(start).is_some_and(u8::is_ascii_digit) {
            return Err(self.unexpected(start, Expected::Digit));
        }
        let mut i = start + 1;
        let mut has_underscore = false;
        while let Some(&b) = bytes.get(i) {
            if b.is_ascii_digit() {
                i += 1;
            } else if b == b'_' {
                has_underscore = true;
                if !bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
                    return Err(self.unexpected(i + 1, Expected::Digit));
                }
                i += 1;
            } else {
                break;
            }
        }
        Ok((i, has_underscore))
    }

    /// Parses an exact `i64`/`u64` integer or finite `f64` float, with optional `_` digit separators.
    fn parse_number(&mut self) -> Result<Value<Alloc<'a>>, ReadError<Alloc<'a>>> {
        let bytes = self.bytes();
        let start = self.at;
        let mut i = start;
        let negative = bytes.get(i) == Some(&b'-');
        if negative {
            i += 1;
        }

        let int_start = i;
        let (int_end, mut has_underscore) = self.scan_digit_run(int_start)?;
        if bytes.get(int_start) == Some(&b'0') && int_end - int_start > 1 {
            return Err(self.err(int_start, What::LeadingZero));
        }
        i = int_end;

        let mut is_float = false;
        if bytes.get(i) == Some(&b'.') {
            is_float = true;
            i += 1;
            let (frac_end, frac_u) = self.scan_digit_run(i)?;
            has_underscore |= frac_u;
            i = frac_end;
        }
        if matches!(bytes.get(i), Some(b'e' | b'E')) {
            is_float = true;
            i += 1;
            if matches!(bytes.get(i), Some(b'+' | b'-')) {
                i += 1;
            }
            let (exp_end, exp_u) = self.scan_digit_run(i)?;
            has_underscore |= exp_u;
            i = exp_end;
        }

        if i - start > self.limits.most_number_len {
            return Err(self.err(start, What::NumberTooLong { most: self.limits.most_number_len }));
        }
        self.at = i;

        if is_float {
            let parsed = if has_underscore {
                let mut clean = String::with_capacity_in(i - start, self.alloc);
                if let Some(raw) = self.text.get(start..i) {
                    for ch in raw.chars() {
                        if ch != '_' {
                            let _ = clean.write_char(ch);
                        }
                    }
                }
                clean.as_str().parse::<f64>()
            } else {
                self.text.get(start..i).unwrap_or("").parse::<f64>()
            };
            return match parsed {
                Ok(f) if f.is_finite() => Ok(Value::Float(f)),
                Ok(_) | Err(_) => Err(self.err(start, What::FloatOutOfRange)),
            };
        }

        let mut magnitude: u64 = 0;
        if let Some(int_bytes) = bytes.get(int_start..int_end) {
            for &b in int_bytes {
                if b == b'_' {
                    continue;
                }
                let digit = u64::from(b - b'0');
                magnitude = match magnitude.checked_mul(10).and_then(|m| m.checked_add(digit)) {
                    Some(m) => m,
                    None => return Err(self.err(start, What::IntegerOutOfRange)),
                };
            }
        }

        match negative {
            false => Ok(Value::Int(Int::from_u64(magnitude))),
            true => match 0i64.checked_sub_unsigned(magnitude) {
                Some(n) => Ok(Value::Int(Int::from_i64(n))),
                None => Err(self.err(start, What::IntegerOutOfRange)),
            },
        }
    }
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn hex4(bytes: &[u8], at: usize) -> Option<u32> {
    let digits = bytes.get(at..at + 4)?;
    let mut value: u32 = 0;
    for &d in digits {
        let nibble = hex_nibble(d)?;
        value = (value << 4) | u32::from(nibble);
    }
    Some(value)
}
