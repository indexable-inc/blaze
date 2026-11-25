//! Interactive multi-line shell editor: cursor & selection, word movement, kill rings,
//! command history with draft preservation, `Ctrl+R` incremental reverse search, syntax
//! highlighting from `blaze_sh::lex::highlight`, and Tab completion powered directly by
//! `blaze_sh::builtins::BUILTIN_SPECS`, active `EngineState` variables/`def`s, flags, and paths.

use std::fs;
use std::path::Path;

use blaze_own::Alloc;
use blaze_sh::EngineState;
use blaze_sh::builtins::{BUILTIN_SPECS, find_spec};
use blaze_sh::lex::{HighlightSpan, highlight, is_incomplete};

/// Kind of tab-completion candidate (used for icon/color badges in the popup).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionKind {
    Builtin,
    Def,
    Keyword,
    Flag,
    Variable,
    Dir,
    File,
}

/// One candidate shown in the floating completion popup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionItem {
    pub label: String,
    pub insert: String,
    pub detail: String,
    pub kind: CompletionKind,
}

/// Incremental `Ctrl+R` reverse history search state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReverseSearch {
    pub query: String,
    pub match_index: Option<usize>,
    pub matched_text: Option<String>,
}

/// Multi-line interactive input editor for `blaze-shell`.
#[derive(Clone, Debug, Default)]
pub struct Editor {
    pub text: String,
    /// Byte offset in `text`, always kept on a valid UTF-8 character boundary.
    pub cursor: usize,
    /// Optional selection anchor byte offset (also a valid UTF-8 boundary).
    pub selection_anchor: Option<usize>,
    /// Submitted command history (oldest first).
    pub history: Vec<String>,
    history_pos: Option<usize>,
    history_draft: String,
    /// Active `Ctrl+R` reverse search overlay, if open.
    pub reverse_search: Option<ReverseSearch>,
    /// Active Tab completion candidates, if open.
    pub completions: Vec<CompletionItem>,
    pub completion_selected: usize,
    completion_replace_start: usize,
}

const KEYWORDS: &[(&str, &str)] = &[
    ("let", "bind immutable variable"),
    ("mut", "bind mutable variable"),
    ("def", "define a custom command"),
    ("if", "conditional branch"),
    ("else", "alternate branch"),
    ("match", "pattern match value"),
    ("for", "iterate over list or range"),
    ("while", "loop while condition holds"),
    ("loop", "infinite loop (exit with break)"),
    ("try", "catch runtime errors"),
    ("rust", "evaluate a live Rust cell via blaze run"),
    ("alias", "define a command alias"),
    ("true", "boolean true"),
    ("false", "boolean false"),
    ("null", "nothing value"),
];

impl Editor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ensure `cursor` and `selection_anchor` are clamped to valid char boundaries.
    fn clamp_cursor(&mut self) {
        self.cursor = clamp_char_boundary(&self.text, self.cursor);
        if let Some(a) = self.selection_anchor {
            self.selection_anchor = Some(clamp_char_boundary(&self.text, a));
        }
    }

    pub fn selection_range(&self) -> Option<(usize, usize)> {
        let a = self.selection_anchor?;
        if a == self.cursor {
            return None;
        }
        Some(if a < self.cursor { (a, self.cursor) } else { (self.cursor, a) })
    }

    pub fn selected_text(&self) -> Option<&str> {
        let (s, e) = self.selection_range()?;
        self.text.get(s..e)
    }

    pub fn select_all(&mut self) {
        if !self.text.is_empty() {
            self.selection_anchor = Some(0);
            self.cursor = self.text.len();
        }
    }

    fn delete_selection(&mut self) -> bool {
        if let Some((s, e)) = self.selection_range() {
            self.text.drain(s..e);
            self.cursor = s;
            self.selection_anchor = None;
            true
        } else {
            self.selection_anchor = None;
            false
        }
    }

    pub fn set_text(&mut self, text: String) {
        self.cursor = text.len();
        self.text = text;
        self.selection_anchor = None;
        self.dismiss_completions();
    }

    pub fn insert_str(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.delete_selection();
        self.clamp_cursor();
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
        self.history_pos = None;
        self.dismiss_completions();
    }

    pub fn insert_char(&mut self, ch: char) {
        let mut buf = [0u8; 4];
        let s = ch.encode_utf8(&mut buf);
        self.insert_str(s);
    }

    pub fn backspace(&mut self) {
        if self.delete_selection() {
            self.history_pos = None;
            self.dismiss_completions();
            return;
        }
        self.clamp_cursor();
        if self.cursor == 0 {
            return;
        }
        let prev = prev_char_boundary(&self.text, self.cursor);
        self.text.drain(prev..self.cursor);
        self.cursor = prev;
        self.history_pos = None;
        self.dismiss_completions();
    }

    pub fn delete_forward(&mut self) {
        if self.delete_selection() {
            self.history_pos = None;
            self.dismiss_completions();
            return;
        }
        self.clamp_cursor();
        if self.cursor >= self.text.len() {
            return;
        }
        let next = next_char_boundary(&self.text, self.cursor);
        self.text.drain(self.cursor..next);
        self.history_pos = None;
        self.dismiss_completions();
    }

    pub fn move_left(&mut self, select: bool) {
        self.clamp_cursor();
        if select {
            self.selection_anchor.get_or_insert(self.cursor);
        } else if let Some((s, _)) = self.selection_range() {
            self.cursor = s;
            self.selection_anchor = None;
            return;
        } else {
            self.selection_anchor = None;
        }
        self.cursor = prev_char_boundary(&self.text, self.cursor);
    }

    pub fn move_right(&mut self, select: bool) {
        self.clamp_cursor();
        if select {
            self.selection_anchor.get_or_insert(self.cursor);
        } else if let Some((_, e)) = self.selection_range() {
            self.cursor = e;
            self.selection_anchor = None;
            return;
        } else {
            self.selection_anchor = None;
        }
        self.cursor = next_char_boundary(&self.text, self.cursor);
    }

    pub fn move_word_left(&mut self, select: bool) {
        self.clamp_cursor();
        if select {
            self.selection_anchor.get_or_insert(self.cursor);
        } else {
            self.selection_anchor = None;
        }
        let mut i = self.cursor;
        while i > 0 {
            let p = prev_char_boundary(&self.text, i);
            let ch = self.text[p..i].chars().next().unwrap_or(' ');
            if !ch.is_whitespace() {
                break;
            }
            i = p;
        }
        while i > 0 {
            let p = prev_char_boundary(&self.text, i);
            let ch = self.text[p..i].chars().next().unwrap_or(' ');
            if ch.is_whitespace() || is_word_sep(ch) {
                break;
            }
            i = p;
        }
        self.cursor = i;
    }

    pub fn move_word_right(&mut self, select: bool) {
        self.clamp_cursor();
        if select {
            self.selection_anchor.get_or_insert(self.cursor);
        } else {
            self.selection_anchor = None;
        }
        let mut i = self.cursor;
        let len = self.text.len();
        while i < len {
            let n = next_char_boundary(&self.text, i);
            let ch = self.text[i..n].chars().next().unwrap_or(' ');
            if !ch.is_whitespace() {
                break;
            }
            i = n;
        }
        while i < len {
            let n = next_char_boundary(&self.text, i);
            let ch = self.text[i..n].chars().next().unwrap_or(' ');
            if ch.is_whitespace() || is_word_sep(ch) {
                break;
            }
            i = n;
        }
        self.cursor = i;
    }

    pub fn move_home(&mut self, select: bool) {
        self.clamp_cursor();
        if select {
            self.selection_anchor.get_or_insert(self.cursor);
        } else {
            self.selection_anchor = None;
        }
        let line_start = self.text[..self.cursor].rfind('\n').map_or(0, |p| p + 1);
        self.cursor = line_start;
    }

    pub fn move_end(&mut self, select: bool) {
        self.clamp_cursor();
        if select {
            self.selection_anchor.get_or_insert(self.cursor);
        } else {
            self.selection_anchor = None;
        }
        let line_end = self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |rel| self.cursor + rel);
        self.cursor = line_end;
    }

    /// Move cursor up one line in a multi-line buffer, or navigate to older command history
    /// when already on the first line (or cycle completion popup if open).
    pub fn move_up(&mut self) {
        if !self.completions.is_empty() {
            if self.completion_selected == 0 {
                self.completion_selected = self.completions.len() - 1;
            } else {
                self.completion_selected -= 1;
            }
            return;
        }
        self.clamp_cursor();
        let before = &self.text[..self.cursor];
        if let Some(cur_nl) = before.rfind('\n') {
            let col_chars = before[cur_nl + 1..].chars().count();
            let prev_start = before[..cur_nl].rfind('\n').map_or(0, |p| p + 1);
            let prev_line = &self.text[prev_start..cur_nl];
            let offset = nth_char_byte_offset(prev_line, col_chars);
            self.cursor = prev_start + offset;
            self.selection_anchor = None;
            return;
        }
        self.history_prev();
    }

    /// Move cursor down one line in a multi-line buffer, or navigate to newer command history
    /// when on the last line (or cycle completion popup if open).
    pub fn move_down(&mut self) {
        if !self.completions.is_empty() {
            self.completion_selected = (self.completion_selected + 1) % self.completions.len();
            return;
        }
        self.clamp_cursor();
        if let Some(rel_nl) = self.text[self.cursor..].find('\n') {
            let line_start = self.text[..self.cursor].rfind('\n').map_or(0, |p| p + 1);
            let col_chars = self.text[line_start..self.cursor].chars().count();
            let next_start = self.cursor + rel_nl + 1;
            let next_end = self.text[next_start..]
                .find('\n')
                .map_or(self.text.len(), |r| next_start + r);
            let next_line = &self.text[next_start..next_end];
            let offset = nth_char_byte_offset(next_line, col_chars);
            self.cursor = next_start + offset;
            self.selection_anchor = None;
            return;
        }
        self.history_next();
    }

    /// `Ctrl+K`: kill from cursor to end of current line.
    pub fn kill_to_end(&mut self) {
        self.clamp_cursor();
        self.selection_anchor = None;
        let end = self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |r| self.cursor + r);
        if end == self.cursor && end < self.text.len() {
            // At newline itself: delete the newline.
            self.text.drain(self.cursor..self.cursor + 1);
        } else {
            self.text.drain(self.cursor..end);
        }
        self.dismiss_completions();
    }

    /// `Ctrl+W`: kill word before cursor.
    pub fn kill_word_back(&mut self) {
        self.clamp_cursor();
        let end = self.cursor;
        self.move_word_left(false);
        let start = self.cursor;
        if start < end {
            self.text.drain(start..end);
        }
        self.dismiss_completions();
    }

    /// `Ctrl+U`: clear entire input buffer.
    pub fn clear_line(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.selection_anchor = None;
        self.history_pos = None;
        self.dismiss_completions();
    }

    pub fn push_history(&mut self, cmd: String) {
        let trimmed = cmd.trim();
        if trimmed.is_empty() {
            return;
        }
        if self.history.last().map(|s| s.as_str()) != Some(trimmed) {
            self.history.push(trimmed.to_owned());
        }
    }

    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next_pos = match self.history_pos {
            None => {
                self.history_draft = self.text.clone();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.history_pos = Some(next_pos);
        self.text = self.history[next_pos].clone();
        self.cursor = self.text.len();
        self.selection_anchor = None;
        self.dismiss_completions();
    }

    pub fn history_next(&mut self) {
        let Some(pos) = self.history_pos else {
            return;
        };
        if pos + 1 < self.history.len() {
            let next = pos + 1;
            self.history_pos = Some(next);
            self.text = self.history[next].clone();
        } else {
            self.history_pos = None;
            self.text = core::mem::take(&mut self.history_draft);
        }
        self.cursor = self.text.len();
        self.selection_anchor = None;
        self.dismiss_completions();
    }

    /// Submit current buffer, recording it in history and resetting input state.
    pub fn take_submitted(&mut self) -> String {
        let submitted = core::mem::take(&mut self.text);
        self.cursor = 0;
        self.selection_anchor = None;
        self.history_pos = None;
        self.history_draft.clear();
        self.reverse_search = None;
        self.dismiss_completions();
        self.push_history(submitted.clone());
        submitted
    }

    /// True if `blaze-sh`'s lexer sees unclosed delimiters/quotes (Enter should insert newline).
    pub fn is_incomplete(&self) -> bool {
        is_incomplete(&self.text)
    }

    /// Lex syntax highlight spans for the current input buffer.
    pub fn highlight_spans(&self) -> Vec<HighlightSpan> {
        highlight(&self.text, Alloc::global()).iter().cloned().collect()
    }

    /// 0-based `(line, col_chars)` of the cursor in `self.text`.
    pub fn cursor_line_col(&self) -> (usize, usize) {
        let c = clamp_char_boundary(&self.text, self.cursor);
        let before = &self.text[..c];
        let line = before.bytes().filter(|&b| b == b'\n').count();
        let line_start = before.rfind('\n').map_or(0, |p| p + 1);
        let col = before[line_start..].chars().count();
        (line, col)
    }

    // -------------------------------------------------------------------------
    // Ctrl+R incremental reverse history search
    // -------------------------------------------------------------------------

    pub fn start_reverse_search(&mut self) {
        if self.reverse_search.is_some() {
            self.reverse_search_next();
            return;
        }
        self.dismiss_completions();
        let mut rs = ReverseSearch {
            query: String::new(),
            match_index: None,
            matched_text: None,
        };
        update_reverse_search(&mut rs, &self.history, self.history.len());
        self.reverse_search = Some(rs);
    }

    pub fn reverse_search_push(&mut self, ch: char) {
        if let Some(rs) = &mut self.reverse_search {
            rs.query.push(ch);
            update_reverse_search(rs, &self.history, self.history.len());
        }
    }

    pub fn reverse_search_backspace(&mut self) {
        if let Some(rs) = &mut self.reverse_search {
            rs.query.pop();
            update_reverse_search(rs, &self.history, self.history.len());
        }
    }

    pub fn reverse_search_next(&mut self) {
        if let Some(rs) = &mut self.reverse_search {
            let start = rs.match_index.unwrap_or(self.history.len());
            if start > 0 {
                update_reverse_search(rs, &self.history, start);
            }
        }
    }

    pub fn accept_reverse_search(&mut self) -> bool {
        let Some(rs) = self.reverse_search.take() else {
            return false;
        };
        if let Some(matched) = rs.matched_text {
            self.set_text(matched);
            true
        } else {
            false
        }
    }

    pub fn cancel_reverse_search(&mut self) {
        self.reverse_search = None;
    }

    // -------------------------------------------------------------------------
    // Tab completion engine
    // -------------------------------------------------------------------------

    pub fn dismiss_completions(&mut self) {
        self.completions.clear();
        self.completion_selected = 0;
    }

    /// Compute or cycle tab completions for the token ending at `self.cursor`.
    pub fn trigger_completion(&mut self, state: &EngineState<'_>) {
        if !self.completions.is_empty() {
            self.completion_selected = (self.completion_selected + 1) % self.completions.len();
            return;
        }
        self.clamp_cursor();
        let before = &self.text[..self.cursor];
        let token_start = before
            .rfind(|c: char| c.is_whitespace() || matches!(c, '|' | ';' | '(' | '[' | '{'))
            .map_or(0, |p| p + 1);
        let prefix = &before[token_start..];
        let prev_non_ws = before[..token_start].trim_end();
        let at_cmd_pos = prev_non_ws.is_empty()
            || prev_non_ws.ends_with('|')
            || prev_non_ws.ends_with(';')
            || prev_non_ws.ends_with('\n');

        let mut items: Vec<CompletionItem> = Vec::new();

        if let Some(var_prefix) = prefix.strip_prefix('$') {
            // Complete variables from EngineState plus `$env` and `$in`.
            for (name, detail) in [("env", "environment record"), ("in", "pipeline input")] {
                if name.starts_with(var_prefix) {
                    items.push(CompletionItem {
                        label: format!("${name}"),
                        insert: format!("${name}"),
                        detail: detail.to_owned(),
                        kind: CompletionKind::Variable,
                    });
                }
            }
            let mut vars: Vec<(String, String)> = Vec::new();
            state.vars.for_each(&mut |k, slot| {
                if k.as_ref().starts_with(var_prefix) {
                    vars.push((k.as_ref().to_owned(), slot.value.type_name().to_owned()));
                }
            });
            vars.sort_by(|a, b| a.0.cmp(&b.0));
            for (name, ty) in vars {
                items.push(CompletionItem {
                    label: format!("${name}"),
                    insert: format!("${name}"),
                    detail: ty,
                    kind: CompletionKind::Variable,
                });
            }
        } else if prefix.starts_with('-') {
            // Complete flags for the active command in the current pipeline stage.
            if let Some(cmd_name) = active_stage_command(before)
                && let Some(spec) = find_spec(cmd_name)
            {
                for &flag in spec.flags {
                    if flag.starts_with(prefix) {
                        items.push(CompletionItem {
                            label: flag.to_owned(),
                            insert: flag.to_owned(),
                            detail: format!("flag of {}", spec.name),
                            kind: CompletionKind::Flag,
                        });
                    }
                }
            }
        } else if at_cmd_pos {
            // Complete builtins, user `def`s, aliases, and language keywords.
            for spec in BUILTIN_SPECS {
                if spec.name.starts_with(prefix) {
                    items.push(CompletionItem {
                        label: spec.name.to_owned(),
                        insert: spec.name.to_owned(),
                        detail: spec.summary.to_owned(),
                        kind: CompletionKind::Builtin,
                    });
                }
            }
            let mut defs: Vec<String> = Vec::new();
            state.defs.for_each(&mut |k, _| {
                if k.as_ref().starts_with(prefix) {
                    defs.push(k.as_ref().to_owned());
                }
            });
            defs.sort();
            for d in defs {
                items.push(CompletionItem {
                    label: d.clone(),
                    insert: d,
                    detail: "user def".to_owned(),
                    kind: CompletionKind::Def,
                });
            }
            for &(kw, desc) in KEYWORDS {
                if kw.starts_with(prefix) {
                    items.push(CompletionItem {
                        label: kw.to_owned(),
                        insert: kw.to_owned(),
                        detail: desc.to_owned(),
                        kind: CompletionKind::Keyword,
                    });
                }
            }
        } else {
            // Complete filesystem paths relative to `state.cwd`.
            collect_path_completions(state.cwd.as_ref(), prefix, &mut items);
        }

        items.truncate(40);
        if items.is_empty() {
            self.dismiss_completions();
            return;
        }
        self.completion_replace_start = token_start;
        self.completions = items;
        self.completion_selected = 0;

        // If there is exactly one candidate, apply it immediately!
        if self.completions.len() == 1 {
            self.apply_selected_completion();
        }
    }

    pub fn apply_selected_completion(&mut self) -> bool {
        self.apply_completion(self.completion_selected)
    }

    pub fn apply_completion(&mut self, idx: usize) -> bool {
        let Some(item) = self.completions.get(idx).cloned() else {
            return false;
        };
        self.clamp_cursor();
        let start = clamp_char_boundary(&self.text, self.completion_replace_start).min(self.cursor);
        let mut insert = item.insert;
        if matches!(item.kind, CompletionKind::Builtin | CompletionKind::Def | CompletionKind::Keyword) {
            let needs_space = !self.text[self.cursor..].starts_with(' ');
            if needs_space {
                insert.push(' ');
            }
        }
        self.text.replace_range(start..self.cursor, &insert);
        self.cursor = start + insert.len();
        self.selection_anchor = None;
        self.dismiss_completions();
        true
    }
}

fn update_reverse_search(rs: &mut ReverseSearch, history: &[String], before_idx: usize) {
    let end = before_idx.min(history.len());
    for i in (0..end).rev() {
        if history[i].contains(&rs.query) {
            rs.match_index = Some(i);
            rs.matched_text = Some(history[i].clone());
            return;
        }
    }
    rs.match_index = None;
    rs.matched_text = None;
}

fn active_stage_command(before_cursor: &str) -> Option<&str> {
    let stage_start = before_cursor
        .rfind(['|', ';', '\n'])
        .map_or(0, |p| p + 1);
    before_cursor[stage_start..].split_whitespace().next()
}

fn collect_path_completions(cwd: &str, prefix: &str, out: &mut Vec<CompletionItem>) {
    let (dir_part, file_prefix) = match prefix.rfind('/') {
        Some(idx) => (&prefix[..=idx], &prefix[idx + 1..]),
        None => ("", prefix),
    };
    let search_dir = if dir_part.is_empty() {
        Path::new(cwd).to_path_buf()
    } else if dir_part.starts_with('/') {
        Path::new(dir_part).to_path_buf()
    } else {
        Path::new(cwd).join(dir_part)
    };
    let Ok(rd) = fs::read_dir(&search_dir) else {
        return;
    };
    let show_hidden = file_prefix.starts_with('.');
    let mut entries: Vec<(String, bool)> = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        if !name.starts_with(file_prefix) {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        entries.push((name, is_dir));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, is_dir) in entries.into_iter().take(40) {
        let label = if is_dir { format!("{name}/") } else { name.clone() };
        let insert = if is_dir {
            format!("{dir_part}{name}/")
        } else {
            format!("{dir_part}{name}")
        };
        out.push(CompletionItem {
            label,
            insert,
            detail: if is_dir { "dir".to_owned() } else { "file".to_owned() },
            kind: if is_dir { CompletionKind::Dir } else { CompletionKind::File },
        });
    }
}

fn is_word_sep(ch: char) -> bool {
    matches!(ch, '/' | '.' | '|' | ';' | '(' | ')' | '[' | ']' | '{' | '}' | '=' | ',')
}

fn clamp_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn prev_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len()).saturating_sub(1);
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn next_char_boundary(s: &str, idx: usize) -> usize {
    let len = s.len();
    if idx >= len {
        return len;
    }
    let mut i = idx + 1;
    while i < len && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn nth_char_byte_offset(line: &str, n: usize) -> usize {
    line.char_indices().nth(n).map_or(line.len(), |(idx, _)| idx)
}
