//! Interactive multi-line shell editor: natural macOS editing (`Cmd+Z`/`Cmd+Shift+Z` undo/redo,
//! word/line navigation, selection, click-drag, kill rings), Fish/Nushell inline ghost
//! autosuggestion (`Right` / `Alt+Right`), IntelliJ-style live fuzzy autocomplete with
//! typed icons, column-aware pipeline suggestions, and `Ctrl+R` reverse history search.

use std::collections::{BTreeMap, HashSet};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use blaze_own::Alloc;
use blaze_sh::builtins::{BUILTIN_SPECS, find_spec};
use blaze_sh::lex::{HighlightSpan, highlight, is_incomplete};
use blaze_sh::{Bytes, EngineState};

use crate::svg_icons::icon_name_for_file;

const MAX_UNDO_DEPTH: usize = 256;
const MAX_PATH_BINS: usize = 4096;

/// Kind of completion candidate (controls icon/badge and insertion spacing).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionKind {
    Abbrev,
    Builtin,
    Def,
    Keyword,
    Flag,
    Variable,
    Column,
    Dir,
    File,
    External,
}

/// One candidate shown in the IntelliJ-style floating completion popup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionItem {
    pub label: String,
    pub insert: String,
    /// Right-aligned compact metadata (type, file size, or kind).
    pub detail: String,
    /// Bottom quick-doc / signature description shown for the highlighted item.
    pub doc: String,
    pub kind: CompletionKind,
    /// Optional vendored SVG icon name (`"folder_macos"`, `"rust_dark"`, `"toml_dark"`, etc.).
    pub icon: Option<&'static str>,
    /// Character indices inside `label` matching the typed query (highlighted IntelliJ-style).
    pub match_indices: Vec<usize>,
}

/// Incremental `Ctrl+R` reverse history search state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReverseSearch {
    pub query: String,
    pub match_index: Option<usize>,
    pub matched_text: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct UndoSnap {
    text: String,
    cursor: usize,
    anchor: Option<usize>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum EditOp {
    #[default]
    Other,
    InsertWord,
    DeleteBack,
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
    /// Active IntelliJ-style completion popup candidates.
    pub completions: Vec<CompletionItem>,
    pub completion_selected: usize,
    completion_replace_start: usize,
    /// Fish/Nushell-style dimmed inline ghost suffix shown directly after the cursor.
    pub ghost: Option<String>,
    /// Bounded undo/redo stacks (`Cmd+Z` / `Cmd+Shift+Z`).
    undo_stack: Vec<UndoSnap>,
    redo_stack: Vec<UndoSnap>,
    last_edit_op: EditOp,
    /// Lazily cached external executable names from `$PATH`.
    path_bins: Option<Vec<String>>,
    /// Command-position abbreviations expanded inline on Space / Enter (like Fish / Nushell).
    pub abbreviations: BTreeMap<String, String>,
}

/// Built-in Fish/Nushell-style command-position abbreviations (`l` -> `ls`, `sb` -> `sort-by`, etc.).
const DEFAULT_ABBREVIATIONS: &[(&str, &str)] = &[
    // Navigation & native structured listing
    ("..", "cd .."),
    ("...", "cd ../.."),
    ("l", "ls"),
    ("ll", "ls -l -a | sort-by modified"),
    ("la", "ls -a"),
    ("lso", "ls | sort-by modified"),
    ("lt", "ls | sort-by modified -r"),
    ("p", "pwd"),
    ("c", "clear"),
    ("o", "open"),
    ("op", "open"),
    // Structured pipeline shorthand (Nushell-style)
    ("w", "where"),
    ("wn", "where name"),
    ("sb", "sort-by"),
    ("sor", "sort-by -r"),
    ("se", "select"),
    ("rj", "reject"),
    ("le", "length"),
    ("col", "columns"),
    ("f1", "first"),
    ("f5", "first 5"),
    ("f9", "first 9"),
    ("l1", "last"),
    ("l5", "last 5"),
    ("fj", "from json"),
    ("tj", "to json"),
    ("u", "uniq"),
    ("cpu", "ps | sort-by cpu -r"),
    // Blaze & Rust
    ("b", "blaze"),
    ("bc", "blaze check"),
    ("bt", "blaze test"),
    ("br", "blaze run"),
    ("be", "blaze export"),
    // Git
    ("gs", "git status"),
    ("gd", "git diff"),
    ("gds", "git diff --staged"),
    ("gl", "git log --oneline"),
    ("ga", "git add"),
    ("ga.", "git add ."),
    ("gc", "git commit"),
    ("gca", "git commit --amend"),
    ("gp", "git push"),
    ("gpl", "git pull"),
    ("gco", "git checkout"),
    ("gb", "git branch"),
    ("gfa", "git fetch --all --prune"),
    ("gwa", "git worktree add"),
    ("gwl", "git worktree list"),
];

const KEYWORDS: &[(&str, &str)] = &[
    ("let", "bind immutable variable: let x = expr"),
    ("mut", "bind mutable variable: mut x = expr"),
    ("def", "define a custom command: def name [arg] { ... }"),
    ("if", "conditional expression: if cond { a } else { b }"),
    ("else", "alternate branch of if"),
    ("match", "pattern match structured value"),
    ("for", "iterate over list or range: for x in 1..10 { ... }"),
    ("while", "loop while condition holds"),
    ("loop", "infinite loop (exit with break)"),
    ("try", "catch runtime errors: try { ... } catch { |e| ... }"),
    ("rust", "evaluate a live Rust cell via blaze run"),
    ("alias", "define a command alias"),
    ("true", "boolean true"),
    ("false", "boolean false"),
    ("null", "nothing value"),
];

const COLUMN_COMMANDS: &[&str] = &[
    "where",
    "sort-by",
    "select",
    "reject",
    "get",
    "group-by",
    "uniq-by",
    "rename",
    "update",
    "insert",
];

impl Editor {
    pub fn new() -> Self {
        let mut ed = Self::default();
        ed.load_default_abbreviations();
        ed
    }

    /// Populate built-in abbreviations.
    pub fn load_default_abbreviations(&mut self) {
        self.abbreviations.clear();
        for &(k, v) in DEFAULT_ABBREVIATIONS {
            self.abbreviations.insert(k.to_owned(), v.to_owned());
        }
    }

    /// Load user-defined `abbreviations.nuon` from Nushell / `blaze-shell` config directories.
    /// Native `blaze-sh` defaults (`l -> ls`, `ll`, `sb`, `w`, `b`, `bc`, etc.) stay preferred
    /// over external `eza` wrappers unless explicitly overridden in `~/.config/blaze-shell/`.
    pub fn load_user_abbreviations(&mut self, home: &str) {
        let home_p = PathBuf::from(home);
        let nu_paths = [
            home_p.join("Library/Application Support/nushell/abbreviations.nuon"),
            home_p.join(".config/nushell/abbreviations.nuon"),
        ];
        for path in &nu_paths {
            if let Ok(raw) = fs::read_to_string(path) {
                for (k, v) in parse_nuon_abbreviations(&raw) {
                    self.abbreviations.entry(k).or_insert(v);
                }
            }
        }
        let blaze_path = home_p.join(".config/blaze-shell/abbreviations.nuon");
        if let Ok(raw) = fs::read_to_string(&blaze_path) {
            for (k, v) in parse_nuon_abbreviations(&raw) {
                self.abbreviations.insert(k, v);
            }
        }
    }

    /// If the cursor is immediately after a command-position abbreviation (e.g. `l`, `sb`, `gs`),
    /// expand it inline and push an undo snapshot so `Cmd+Z` reverts the expansion cleanly.
    pub fn try_expand_abbreviation(&mut self) -> bool {
        if self.selection_range().is_some() {
            return false;
        }
        self.clamp_cursor();
        let before = &self.text[..self.cursor];
        let token_start = token_start_offset(before);
        let token = &before[token_start..];
        if token.is_empty() {
            return false;
        }
        let prev_non_ws = before[..token_start].trim_end();
        let at_cmd_pos = prev_non_ws.is_empty()
            || prev_non_ws.ends_with('|')
            || prev_non_ws.ends_with(';')
            || prev_non_ws.ends_with('\n');
        if !at_cmd_pos {
            return false;
        }
        let Some(expansion) = self.abbreviations.get(token).cloned() else {
            return false;
        };
        if expansion == token {
            return false;
        }
        self.push_undo(EditOp::Other);
        self.text.replace_range(token_start..self.cursor, &expansion);
        self.cursor = token_start + expansion.len();
        self.dismiss_completions();
        self.ghost = None;
        true
    }

    fn clamp_cursor(&mut self) {
        self.cursor = clamp_char_boundary(&self.text, self.cursor);
        if let Some(a) = self.selection_anchor {
            self.selection_anchor = Some(clamp_char_boundary(&self.text, a));
        }
    }

    fn push_undo(&mut self, op: EditOp) {
        let coalesce = op != EditOp::Other && op == self.last_edit_op && !self.undo_stack.is_empty();
        if !coalesce {
            let snap = UndoSnap {
                text: self.text.clone(),
                cursor: self.cursor,
                anchor: self.selection_anchor,
            };
            if self.undo_stack.last() != Some(&snap) {
                if self.undo_stack.len() >= MAX_UNDO_DEPTH {
                    self.undo_stack.remove(0);
                }
                self.undo_stack.push(snap);
            }
        }
        self.redo_stack.clear();
        self.last_edit_op = op;
    }

    /// `Cmd+Z`: restore previous editor state.
    pub fn undo(&mut self) -> bool {
        let Some(prev) = self.undo_stack.pop() else {
            return false;
        };
        let cur = UndoSnap {
            text: core::mem::take(&mut self.text),
            cursor: self.cursor,
            anchor: self.selection_anchor,
        };
        if self.redo_stack.len() >= MAX_UNDO_DEPTH {
            self.redo_stack.remove(0);
        }
        self.redo_stack.push(cur);
        self.text = prev.text;
        self.cursor = prev.cursor;
        self.selection_anchor = prev.anchor;
        self.clamp_cursor();
        self.last_edit_op = EditOp::Other;
        self.history_pos = None;
        self.dismiss_completions();
        self.ghost = None;
        true
    }

    /// `Cmd+Shift+Z`: reapply an undone edit.
    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo_stack.pop() else {
            return false;
        };
        let cur = UndoSnap {
            text: core::mem::take(&mut self.text),
            cursor: self.cursor,
            anchor: self.selection_anchor,
        };
        if self.undo_stack.len() >= MAX_UNDO_DEPTH {
            self.undo_stack.remove(0);
        }
        self.undo_stack.push(cur);
        self.text = next.text;
        self.cursor = next.cursor;
        self.selection_anchor = next.anchor;
        self.clamp_cursor();
        self.last_edit_op = EditOp::Other;
        self.history_pos = None;
        self.dismiss_completions();
        self.ghost = None;
        true
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
            self.last_edit_op = EditOp::Other;
        }
    }

    /// `Cmd+X`: cut active selection (or entire line if nothing selected) and return cut text.
    pub fn cut_selection(&mut self) -> Option<String> {
        if let Some((s, e)) = self.selection_range() {
            let cut = self.text[s..e].to_owned();
            self.push_undo(EditOp::Other);
            self.text.drain(s..e);
            self.cursor = s;
            self.selection_anchor = None;
            self.history_pos = None;
            self.dismiss_completions();
            Some(cut)
        } else if !self.text.is_empty() {
            let cut = self.text.clone();
            self.push_undo(EditOp::Other);
            self.text.clear();
            self.cursor = 0;
            self.selection_anchor = None;
            self.history_pos = None;
            self.dismiss_completions();
            Some(cut)
        } else {
            None
        }
    }

    fn delete_selection_raw(&mut self) -> bool {
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
        if self.text != text {
            self.push_undo(EditOp::Other);
        }
        self.cursor = text.len();
        self.text = text;
        self.selection_anchor = None;
        self.dismiss_completions();
        self.ghost = None;
    }

    /// Set text silently (for demo initialization without polluting undo history).
    pub fn set_text_initial(&mut self, text: String) {
        self.cursor = text.len();
        self.text = text;
        self.selection_anchor = None;
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.last_edit_op = EditOp::Other;
        self.dismiss_completions();
    }

    pub fn insert_str(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        let has_sel = self.selection_range().is_some();
        let is_single_word_char =
            !has_sel && s.chars().count() == 1 && !s.chars().next().unwrap().is_whitespace();
        let op = if is_single_word_char { EditOp::InsertWord } else { EditOp::Other };
        self.push_undo(op);
        self.delete_selection_raw();
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

    /// Insert a drag-and-dropped file/folder/value token with automatic shell quoting and spacing.
    pub fn insert_dropped_token(&mut self, raw_token: &str) {
        let trimmed = raw_token.trim();
        if trimmed.is_empty() {
            return;
        }
        let quoted = shell_quote_if_needed(trimmed);
        self.push_undo(EditOp::Other);
        self.delete_selection_raw();
        self.clamp_cursor();

        let needs_leading_space = self.cursor > 0
            && !self.text[..self.cursor]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_whitespace() || matches!(c, '(' | '[' | '{' | '='));
        let needs_trailing_space = !self.text[self.cursor..].starts_with(' ');

        let mut ins = String::with_capacity(quoted.len() + 2);
        if needs_leading_space {
            ins.push(' ');
        }
        ins.push_str(&quoted);
        if needs_trailing_space {
            ins.push(' ');
        }
        self.text.insert_str(self.cursor, &ins);
        self.cursor += ins.len();
        self.history_pos = None;
        self.dismiss_completions();
    }

    pub fn backspace(&mut self) {
        if self.selection_range().is_some() {
            self.push_undo(EditOp::Other);
            self.delete_selection_raw();
            self.history_pos = None;
            self.dismiss_completions();
            return;
        }
        self.clamp_cursor();
        if self.cursor == 0 {
            return;
        }
        self.push_undo(EditOp::DeleteBack);
        let prev = prev_char_boundary(&self.text, self.cursor);
        self.text.drain(prev..self.cursor);
        self.cursor = prev;
        self.history_pos = None;
        self.dismiss_completions();
    }

    pub fn delete_forward(&mut self) {
        if self.selection_range().is_some() {
            self.push_undo(EditOp::Other);
            self.delete_selection_raw();
            self.history_pos = None;
            self.dismiss_completions();
            return;
        }
        self.clamp_cursor();
        if self.cursor >= self.text.len() {
            return;
        }
        self.push_undo(EditOp::Other);
        let next = next_char_boundary(&self.text, self.cursor);
        self.text.drain(self.cursor..next);
        self.history_pos = None;
        self.dismiss_completions();
    }

    pub fn move_left(&mut self, select: bool) {
        self.clamp_cursor();
        self.last_edit_op = EditOp::Other;
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
        self.last_edit_op = EditOp::Other;
        if !select && self.selection_range().is_none() && self.cursor == self.text.len() {
            if self.accept_ghost_full() {
                return;
            }
        }
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
        self.last_edit_op = EditOp::Other;
        if select {
            self.selection_anchor.get_or_insert(self.cursor);
        } else {
            self.selection_anchor = None;
        }
        self.cursor = word_left_boundary(&self.text, self.cursor);
    }

    pub fn move_word_right(&mut self, select: bool) {
        self.clamp_cursor();
        self.last_edit_op = EditOp::Other;
        if !select && self.selection_range().is_none() && self.cursor == self.text.len() {
            if self.accept_ghost_word() {
                return;
            }
        }
        if select {
            self.selection_anchor.get_or_insert(self.cursor);
        } else {
            self.selection_anchor = None;
        }
        self.cursor = word_right_boundary(&self.text, self.cursor);
    }

    pub fn move_home(&mut self, select: bool) {
        self.clamp_cursor();
        self.last_edit_op = EditOp::Other;
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
        self.last_edit_op = EditOp::Other;
        if !select && self.selection_range().is_none() && self.cursor == self.text.len() {
            if self.accept_ghost_full() {
                return;
            }
        }
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

    /// Position cursor from mouse click/drag inside the composer (`line`, `col` 0-based).
    pub fn set_cursor_at_line_col(&mut self, target_line: usize, target_col: usize, select: bool) {
        self.clamp_cursor();
        self.last_edit_op = EditOp::Other;
        if select {
            self.selection_anchor.get_or_insert(self.cursor);
        } else {
            self.selection_anchor = None;
        }
        let mut line_start = 0usize;
        let mut cur_line = 0usize;
        for part in self.text.split('\n') {
            if cur_line == target_line {
                let off = nth_char_byte_offset(part, target_col);
                self.cursor = line_start + off;
                return;
            }
            line_start += part.len() + 1;
            cur_line += 1;
        }
        self.cursor = self.text.len();
    }

    /// Double-click word selection at `(line, col)` inside the composer.
    pub fn select_word_at(&mut self, target_line: usize, target_col: usize) {
        self.set_cursor_at_line_col(target_line, target_col, false);
        if self.text.is_empty() {
            return;
        }
        let mut s = self.cursor;
        let mut e = self.cursor;
        while s > 0 {
            let p = prev_char_boundary(&self.text, s);
            let ch = self.text[p..s].chars().next().unwrap_or(' ');
            if ch.is_whitespace() || is_word_sep(ch) {
                break;
            }
            s = p;
        }
        while e < self.text.len() {
            let n = next_char_boundary(&self.text, e);
            let ch = self.text[e..n].chars().next().unwrap_or(' ');
            if ch.is_whitespace() || is_word_sep(ch) {
                break;
            }
            e = n;
        }
        if s < e {
            self.selection_anchor = Some(s);
            self.cursor = e;
        }
    }

    /// Move cursor up one line in a multi-line buffer, navigate older command history,
    /// or cycle the completion popup if open.
    pub fn move_up(&mut self) {
        self.last_edit_op = EditOp::Other;
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

    /// Move cursor down one line in a multi-line buffer, navigate newer command history,
    /// or cycle the completion popup if open.
    pub fn move_down(&mut self) {
        self.last_edit_op = EditOp::Other;
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
        let end = self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |r| self.cursor + r);
        if self.cursor == end && end >= self.text.len() {
            return;
        }
        self.push_undo(EditOp::Other);
        self.selection_anchor = None;
        if end == self.cursor && end < self.text.len() {
            self.text.drain(self.cursor..self.cursor + 1);
        } else {
            self.text.drain(self.cursor..end);
        }
        self.dismiss_completions();
    }

    /// `Cmd+Backspace`: kill from start of current line to cursor.
    pub fn kill_to_line_start(&mut self) {
        if self.selection_range().is_some() {
            self.push_undo(EditOp::Other);
            self.delete_selection_raw();
            self.dismiss_completions();
            return;
        }
        self.clamp_cursor();
        let line_start = self.text[..self.cursor].rfind('\n').map_or(0, |p| p + 1);
        if line_start < self.cursor {
            self.push_undo(EditOp::Other);
            self.text.drain(line_start..self.cursor);
            self.cursor = line_start;
            self.selection_anchor = None;
            self.history_pos = None;
            self.dismiss_completions();
        }
    }

    /// `Ctrl+W` / `Alt+Backspace`: kill word before cursor.
    pub fn kill_word_back(&mut self) {
        if self.selection_range().is_some() {
            self.push_undo(EditOp::Other);
            self.delete_selection_raw();
            self.dismiss_completions();
            return;
        }
        self.clamp_cursor();
        let end = self.cursor;
        let start = word_left_boundary(&self.text, end);
        if start < end {
            self.push_undo(EditOp::Other);
            self.text.drain(start..end);
            self.cursor = start;
            self.selection_anchor = None;
            self.history_pos = None;
        }
        self.dismiss_completions();
    }

    /// `Alt+Delete`: kill word after cursor.
    pub fn kill_word_forward(&mut self) {
        if self.selection_range().is_some() {
            self.push_undo(EditOp::Other);
            self.delete_selection_raw();
            self.dismiss_completions();
            return;
        }
        self.clamp_cursor();
        let start = self.cursor;
        let end = word_right_boundary(&self.text, start);
        if start < end {
            self.push_undo(EditOp::Other);
            self.text.drain(start..end);
            self.selection_anchor = None;
            self.history_pos = None;
        }
        self.dismiss_completions();
    }

    /// `Ctrl+U`: clear entire input buffer.
    pub fn clear_line(&mut self) {
        if !self.text.is_empty() {
            self.push_undo(EditOp::Other);
        }
        self.text.clear();
        self.cursor = 0;
        self.selection_anchor = None;
        self.history_pos = None;
        self.dismiss_completions();
        self.ghost = None;
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
        self.ghost = None;
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
        self.ghost = None;
    }

    /// Submit current buffer, recording it in history and resetting input state.
    pub fn take_submitted(&mut self) -> String {
        let submitted = core::mem::take(&mut self.text);
        self.cursor = 0;
        self.selection_anchor = None;
        self.history_pos = None;
        self.history_draft.clear();
        self.reverse_search = None;
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.last_edit_op = EditOp::Other;
        self.dismiss_completions();
        self.ghost = None;
        self.push_history(submitted.clone());
        submitted
    }

    pub fn is_incomplete(&self) -> bool {
        is_incomplete(&self.text)
    }

    pub fn highlight_spans(&self) -> Vec<HighlightSpan> {
        highlight(&self.text, Alloc::global()).iter().cloned().collect()
    }

    pub fn cursor_line_col(&self) -> (usize, usize) {
        let c = clamp_char_boundary(&self.text, self.cursor);
        let before = &self.text[..c];
        let line = before.bytes().filter(|&b| b == b'\n').count();
        let line_start = before.rfind('\n').map_or(0, |p| p + 1);
        let col = before[line_start..].chars().count();
        (line, col)
    }

    // -------------------------------------------------------------------------
    // Fish / Nushell inline ghost autosuggestion
    // -------------------------------------------------------------------------

    /// Accept the entire Fish-style ghost suggestion (`Right` / `End` at end of line).
    pub fn accept_ghost_full(&mut self) -> bool {
        let Some(suffix) = self.ghost.take() else {
            return false;
        };
        if suffix.is_empty() {
            return false;
        }
        self.push_undo(EditOp::Other);
        self.clamp_cursor();
        self.text.insert_str(self.cursor, &suffix);
        self.cursor += suffix.len();
        self.selection_anchor = None;
        self.dismiss_completions();
        true
    }

    /// Accept the next word of the Fish-style ghost suggestion (`Alt+Right` at end of line).
    pub fn accept_ghost_word(&mut self) -> bool {
        let Some(suffix) = self.ghost.clone() else {
            return false;
        };
        if suffix.is_empty() {
            return false;
        }
        let end = word_right_boundary(&suffix, 0).max(next_char_boundary(&suffix, 0));
        let piece = &suffix[..end];
        let rest = &suffix[end..];
        self.push_undo(EditOp::Other);
        self.clamp_cursor();
        self.text.insert_str(self.cursor, piece);
        self.cursor += piece.len();
        self.selection_anchor = None;
        self.ghost = if rest.is_empty() { None } else { Some(rest.to_owned()) };
        self.dismiss_completions();
        true
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
    // IntelliJ-style live autocomplete + Fish ghost autosuggestion
    // -------------------------------------------------------------------------

    pub fn dismiss_completions(&mut self) {
        self.completions.clear();
        self.completion_selected = 0;
    }

    fn ensure_path_bins(&mut self) {
        if self.path_bins.is_some() {
            return;
        }
        let mut seen: HashSet<String> = HashSet::new();
        let mut bins: Vec<String> = Vec::new();
        if let Ok(path_var) = std::env::var("PATH") {
            for dir in path_var.split(':') {
                if dir.is_empty() {
                    continue;
                }
                let Ok(rd) = fs::read_dir(dir) else { continue };
                for entry in rd.flatten() {
                    if bins.len() >= MAX_PATH_BINS {
                        break;
                    }
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.is_empty() || name.starts_with('.') || seen.contains(&name) {
                        continue;
                    }
                    let is_exec = entry
                        .metadata()
                        .map(|m| {
                            #[cfg(unix)]
                            {
                                m.is_file() && (m.permissions().mode() & 0o111 != 0)
                            }
                            #[cfg(not(unix))]
                            {
                                m.is_file()
                            }
                        })
                        .unwrap_or(false);
                    if is_exec {
                        seen.insert(name.clone());
                        bins.push(name);
                    }
                }
            }
        }
        bins.sort_unstable();
        self.path_bins = Some(bins);
    }

    /// Refresh Fish-style inline ghost text and (when `open_popup` is true) live IntelliJ
    /// completion candidates as the user types — never auto-inserting without confirmation.
    pub fn refresh_assist(
        &mut self,
        state: &EngineState<'_>,
        recent_cols: &[String],
        open_popup: bool,
    ) {
        self.clamp_cursor();
        self.ghost = None;

        if self.text.is_empty() || self.reverse_search.is_some() {
            self.dismiss_completions();
            return;
        }

        // 1. Fish-style history prefix ghost when cursor is at end of single-line buffer.
        let at_eol = self.cursor == self.text.len() && !self.text.contains('\n');
        if at_eol {
            for hist in self.history.iter().rev() {
                if hist.len() > self.text.len() && hist.starts_with(&self.text) {
                    self.ghost = Some(hist[self.text.len()..].to_owned());
                    break;
                }
            }
        }

        if !open_popup {
            return;
        }

        let before = &self.text[..self.cursor];
        let token_start = token_start_offset(before);
        let prefix = before[token_start..].to_owned();
        let prev_non_ws = before[..token_start].trim_end();

        // Trigger live IntelliJ popup when typing a token (>= 1 char), `$`, `-`, `/`, or after `| `.
        let should_show = !prefix.is_empty()
            || prev_non_ws.ends_with('|')
            || (before.ends_with(' ') && expects_column_arg(before));

        if !should_show {
            self.dismiss_completions();
            return;
        }

        let items = self.compute_completions(state, recent_cols, token_start, &prefix);
        // Hide popup if the only candidate is already typed verbatim.
        if items.len() == 1 && items[0].insert == prefix {
            self.dismiss_completions();
            return;
        }
        if let Some(first) = items.first()
            && self.ghost.is_none()
            && at_eol
            && !prefix.is_empty()
            && first.insert.len() > prefix.len()
            && first.insert.starts_with(&prefix)
        {
            self.ghost = Some(first.insert[prefix.len()..].to_owned());
        }

        if items.is_empty() {
            self.dismiss_completions();
        } else {
            self.completion_replace_start = token_start;
            self.completions = items;
            self.completion_selected = 0;
        }
    }

    /// Explicit `Tab` / `Ctrl+Space` completion: cycles open popup or opens & auto-applies
    /// if exactly one candidate matches.
    pub fn trigger_completion(&mut self, state: &EngineState<'_>) {
        self.trigger_completion_with_cols(state, &[]);
    }

    pub fn trigger_completion_with_cols(
        &mut self,
        state: &EngineState<'_>,
        recent_cols: &[String],
    ) {
        if !self.completions.is_empty() {
            if self.completions.len() == 1 {
                self.apply_selected_completion();
            } else {
                self.completion_selected = (self.completion_selected + 1) % self.completions.len();
            }
            return;
        }
        self.clamp_cursor();
        let before = &self.text[..self.cursor];
        let token_start = token_start_offset(before);
        let prefix = before[token_start..].to_owned();
        let items = self.compute_completions(state, recent_cols, token_start, &prefix);
        if items.is_empty() {
            self.dismiss_completions();
            return;
        }
        self.completion_replace_start = token_start;
        self.completions = items;
        self.completion_selected = 0;

        if self.completions.len() == 1 {
            self.apply_selected_completion();
        }
    }

    fn compute_completions(
        &mut self,
        state: &EngineState<'_>,
        recent_cols: &[String],
        token_start: usize,
        prefix: &str,
    ) -> Vec<CompletionItem> {
        let before = &self.text[..self.cursor];
        let prev_non_ws = before[..token_start].trim_end();
        let at_cmd_pos = prev_non_ws.is_empty()
            || prev_non_ws.ends_with('|')
            || prev_non_ws.ends_with(';')
            || prev_non_ws.ends_with('\n');

        let mut scored: Vec<(i32, CompletionItem)> = Vec::new();

        if let Some(var_prefix) = prefix.strip_prefix('$') {
            for (name, detail, doc) in [
                ("env", "record", "Environment variables record ($env.PATH, $env.HOME)"),
                ("in", "any", "Value piped from the previous stage"),
            ] {
                if let Some((score, mut idxs)) = fuzzy_match_score(name, var_prefix) {
                    for i in &mut idxs {
                        *i += 1; // offset past `$`
                    }
                    scored.push((
                        score + 20,
                        CompletionItem {
                            label: format!("${name}"),
                            insert: format!("${name}"),
                            detail: detail.to_owned(),
                            doc: doc.to_owned(),
                            kind: CompletionKind::Variable,
                            icon: None,
                            match_indices: idxs,
                        },
                    ));
                }
            }
            state.vars.for_each(&mut |k, slot| {
                if let Some((score, mut idxs)) = fuzzy_match_score(k.as_ref(), var_prefix) {
                    for i in &mut idxs {
                        *i += 1;
                    }
                    let ty = slot.value.type_name().to_owned();
                    scored.push((
                        score + 10,
                        CompletionItem {
                            label: format!("${}", k.as_ref()),
                            insert: format!("${}", k.as_ref()),
                            detail: ty.clone(),
                            doc: format!("session variable · {ty}"),
                            kind: CompletionKind::Variable,
                            icon: None,
                            match_indices: idxs,
                        },
                    ));
                }
            });
        } else if prefix.starts_with('-') {
            if let Some(cmd_name) = active_stage_command(before)
                && let Some(spec) = find_spec(cmd_name)
            {
                for &flag in spec.flags {
                    if let Some((score, idxs)) = fuzzy_match_score(flag, prefix) {
                        scored.push((
                            score + 30,
                            CompletionItem {
                                label: flag.to_owned(),
                                insert: flag.to_owned(),
                                detail: "flag".to_owned(),
                                doc: format!("{} {flag} — {}", spec.name, spec.summary),
                                kind: CompletionKind::Flag,
                                icon: None,
                                match_indices: idxs,
                            },
                        ));
                    }
                }
            }
        } else if at_cmd_pos {
            if !prefix.is_empty() {
                for (abbr, exp) in &self.abbreviations {
                    if let Some((score, idxs)) = fuzzy_match_score(abbr, prefix) {
                        let exact_boost = if abbr == prefix { 80 } else { 35 };
                        scored.push((
                            score + exact_boost,
                            CompletionItem {
                                label: abbr.clone(),
                                insert: exp.clone(),
                                detail: format!("→ {exp}"),
                                doc: format!("abbr `{abbr}` → `{exp}` (expands on Space/Enter)"),
                                kind: CompletionKind::Abbrev,
                                icon: None,
                                match_indices: idxs,
                            },
                        ));
                    }
                }
            }
            for spec in BUILTIN_SPECS {
                if let Some((score, idxs)) = fuzzy_match_score(spec.name, prefix) {
                    scored.push((
                        score + 40,
                        CompletionItem {
                            label: spec.name.to_owned(),
                            insert: spec.name.to_owned(),
                            detail: "builtin".to_owned(),
                            doc: format!("{} — {}", spec.name, spec.summary),
                            kind: CompletionKind::Builtin,
                            icon: None,
                            match_indices: idxs,
                        },
                    ));
                }
            }
            state.defs.for_each(&mut |k, _| {
                if let Some((score, idxs)) = fuzzy_match_score(k.as_ref(), prefix) {
                    scored.push((
                        score + 45,
                        CompletionItem {
                            label: k.as_ref().to_owned(),
                            insert: k.as_ref().to_owned(),
                            detail: "def".to_owned(),
                            doc: format!("user command `{}`", k.as_ref()),
                            kind: CompletionKind::Def,
                            icon: None,
                            match_indices: idxs,
                        },
                    ));
                }
            });
            for &(kw, desc) in KEYWORDS {
                if let Some((score, idxs)) = fuzzy_match_score(kw, prefix) {
                    scored.push((
                        score + 30,
                        CompletionItem {
                            label: kw.to_owned(),
                            insert: kw.to_owned(),
                            detail: "keyword".to_owned(),
                            doc: desc.to_owned(),
                            kind: CompletionKind::Keyword,
                            icon: if kw == "rust" { Some("rust_dark") } else { None },
                            match_indices: idxs,
                        },
                    ));
                }
            }
            // External PATH commands (only when user has typed >= 1 char so empty `| ` stays clean).
            if !prefix.is_empty() {
                self.ensure_path_bins();
                if let Some(bins) = &self.path_bins {
                    let mut ext_added = 0usize;
                    for bin in bins {
                        if let Some((score, idxs)) = fuzzy_match_score(bin, prefix) {
                            scored.push((
                                score,
                                CompletionItem {
                                    label: bin.clone(),
                                    insert: bin.clone(),
                                    detail: "bin".to_owned(),
                                    doc: format!("external command `{bin}` on $PATH"),
                                    kind: CompletionKind::External,
                                    icon: Some("shell_dark"),
                                    match_indices: idxs,
                                },
                            ));
                            ext_added += 1;
                            if ext_added >= 24 {
                                break;
                            }
                        }
                    }
                }
            }
        } else {
            // Column-aware completion for table pipeline operators (`where`, `sort-by`, `select`, `get`, ...).
            if expects_column_arg(before) {
                let cols = infer_pipeline_columns(before, recent_cols);
                for (col_name, col_ty) in cols {
                    if let Some((score, idxs)) = fuzzy_match_score(&col_name, prefix) {
                        scored.push((
                            score + 50,
                            CompletionItem {
                                label: col_name.clone(),
                                insert: col_name.clone(),
                                detail: format!("col · {col_ty}"),
                                doc: format!("table column `{col_name}` ({col_ty})"),
                                kind: CompletionKind::Column,
                                icon: None,
                                match_indices: idxs,
                            },
                        ));
                    }
                }
            }
            // Filesystem completion with macOS folder + IntelliJ New UI dark SVG icons.
            collect_path_completions(state.cwd.as_ref(), prefix, &mut scored);
        }

        // Highest match score first, then alphabetical by label.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.label.cmp(&b.1.label)));

        // Deduplicate by label (keeping highest-ranked entry).
        let mut seen_labels: HashSet<String> = HashSet::new();
        let mut items: Vec<CompletionItem> = Vec::new();
        for (_, item) in scored {
            if seen_labels.insert(item.label.clone()) {
                items.push(item);
                if items.len() >= 36 {
                    break;
                }
            }
        }
        items
    }

    pub fn apply_selected_completion(&mut self) -> bool {
        self.apply_completion(self.completion_selected)
    }

    pub fn apply_completion(&mut self, idx: usize) -> bool {
        let Some(item) = self.completions.get(idx).cloned() else {
            return false;
        };
        self.push_undo(EditOp::Other);
        self.clamp_cursor();
        let start = clamp_char_boundary(&self.text, self.completion_replace_start).min(self.cursor);
        let mut insert = item.insert;
        if matches!(
            item.kind,
            CompletionKind::Abbrev
                | CompletionKind::Builtin
                | CompletionKind::Def
                | CompletionKind::Keyword
                | CompletionKind::External
                | CompletionKind::Column
        ) {
            let needs_space = !self.text[self.cursor..].starts_with(' ');
            if needs_space {
                insert.push(' ');
            }
        }
        self.text.replace_range(start..self.cursor, &insert);
        self.cursor = start + insert.len();
        self.selection_anchor = None;
        self.dismiss_completions();
        self.ghost = None;
        true
    }
}

/// Fuzzy + prefix subsequence matcher returning `(score, matched_char_indices)`.
/// Prefix matches always beat interior/fuzzy matches; word-boundary matches (after `-`, `_`, `/`, `.`, or camelCase) get a strong bonus.
pub fn fuzzy_match_score(candidate: &str, query: &str) -> Option<(i32, Vec<usize>)> {
    if query.is_empty() {
        return Some((100, Vec::new()));
    }
    let cand_chars: Vec<char> = candidate.chars().collect();
    let query_chars: Vec<char> = query.chars().collect();
    if query_chars.len() > cand_chars.len() {
        return None;
    }

    // Fast exact/case-insensitive prefix match gets top score.
    let prefix_exact = cand_chars
        .iter()
        .zip(query_chars.iter())
        .all(|(c, q)| c.eq_ignore_ascii_case(q));
    if prefix_exact {
        let case_bonus = if candidate.starts_with(query) { 30 } else { 15 };
        let len_penalty = (cand_chars.len() - query_chars.len()).min(40) as i32;
        return Some((220 + case_bonus - len_penalty, (0..query_chars.len()).collect()));
    }

    // Subsequence match preferring word boundaries and consecutive runs.
    let mut indices = Vec::with_capacity(query_chars.len());
    let mut score = 80i32;
    let mut qi = 0usize;
    let mut prev_match: Option<usize> = None;

    for (ci, &ch) in cand_chars.iter().enumerate() {
        if qi >= query_chars.len() {
            break;
        }
        if ch.eq_ignore_ascii_case(&query_chars[qi]) {
            indices.push(ci);
            if prev_match == Some(ci.saturating_sub(1)) {
                score += 18;
            }
            let is_boundary = ci == 0
                || matches!(cand_chars[ci - 1], '-' | '_' | '/' | '.' | ' ')
                || (ch.is_uppercase() && cand_chars[ci - 1].is_lowercase());
            if is_boundary {
                score += 22;
            }
            if ch == query_chars[qi] {
                score += 4;
            }
            prev_match = Some(ci);
            qi += 1;
        }
    }

    if qi == query_chars.len() {
        let len_penalty = (cand_chars.len() - query_chars.len()).min(50) as i32;
        Some((score - len_penalty, indices))
    } else {
        None
    }
}

/// Parse a Nushell/blaze-shell `abbreviations.nuon` `{ "abbr": "expansion" }` map.
pub fn parse_nuon_abbreviations(src: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in src.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed == "{" || trimmed == "}" {
            continue;
        }
        let Some((key, rest)) = parse_quoted_str(trimmed) else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(after_colon) = rest.strip_prefix(':') else {
            continue;
        };
        let after_colon = after_colon.trim_start();
        let Some((val, _)) = parse_quoted_str(after_colon) else {
            continue;
        };
        if !key.is_empty() && !val.is_empty() {
            out.push((key, val));
        }
    }
    out
}

fn parse_quoted_str(s: &str) -> Option<(String, &str)> {
    let mut chars = s.char_indices();
    let (_, open) = chars.next()?;
    if open != '"' && open != '\'' {
        return None;
    }
    let mut buf = String::new();
    let mut escaped = false;
    for (idx, ch) in chars {
        if escaped {
            match ch {
                'n' => buf.push('\n'),
                't' => buf.push('\t'),
                'r' => buf.push('\r'),
                other => buf.push(other),
            }
            escaped = false;
        } else if ch == '\\' && open == '"' {
            escaped = true;
        } else if ch == open {
            return Some((buf, &s[idx + ch.len_utf8()..]));
        } else {
            buf.push(ch);
        }
    }
    None
}

/// Shell-quote a token when it contains whitespace or shell metacharacters.
pub fn shell_quote_if_needed(s: &str) -> String {
    let needs_quote = s.is_empty()
        || s.chars().any(|c| {
            c.is_whitespace() || matches!(c, '|' | ';' | '(' | ')' | '[' | ']' | '{' | '}' | '"' | '\'' | '$')
        });
    if !needs_quote {
        return s.to_owned();
    }
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn token_start_offset(before: &str) -> usize {
    before
        .rfind(|c: char| c.is_whitespace() || matches!(c, '|' | ';' | '(' | '[' | '{'))
        .map_or(0, |p| p + 1)
}

fn expects_column_arg(before_cursor: &str) -> bool {
    let Some(cmd) = active_stage_command(before_cursor) else {
        return false;
    };
    COLUMN_COMMANDS.contains(&cmd)
}

fn infer_pipeline_columns(before_cursor: &str, recent_cols: &[String]) -> Vec<(String, String)> {
    // Look at the upstream stage command before the last `|`.
    if let Some(pipe_idx) = before_cursor.rfind('|') {
        let upstream = &before_cursor[..pipe_idx];
        let first_cmd = upstream
            .rsplit([';', '\n'])
            .next()
            .unwrap_or(upstream)
            .split('|')
            .next()
            .unwrap_or("")
            .split_whitespace()
            .next()
            .unwrap_or("");
        match first_cmd {
            "ls" => {
                return vec![
                    ("name".to_owned(), "file".to_owned()),
                    ("type".to_owned(), "string".to_owned()),
                    ("size".to_owned(), "filesize".to_owned()),
                    ("modified".to_owned(), "datetime".to_owned()),
                ];
            }
            "ps" => {
                return vec![
                    ("pid".to_owned(), "int".to_owned()),
                    ("name".to_owned(), "process".to_owned()),
                    ("status".to_owned(), "status".to_owned()),
                    ("cpu".to_owned(), "float".to_owned()),
                    ("mem".to_owned(), "filesize".to_owned()),
                ];
            }
            "sys" => {
                return vec![
                    ("host".to_owned(), "string".to_owned()),
                    ("os".to_owned(), "string".to_owned()),
                    ("arch".to_owned(), "string".to_owned()),
                    ("cpus".to_owned(), "int".to_owned()),
                    ("mem_total".to_owned(), "filesize".to_owned()),
                    ("mem_used".to_owned(), "filesize".to_owned()),
                    ("uptime".to_owned(), "duration".to_owned()),
                ];
            }
            "env" => {
                return vec![
                    ("name".to_owned(), "string".to_owned()),
                    ("value".to_owned(), "string".to_owned()),
                ];
            }
            _ => {}
        }
    }
    // Fall back to columns of the most recently produced table in the session!
    recent_cols
        .iter()
        .map(|c| (c.clone(), "col".to_owned()))
        .collect()
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

fn collect_path_completions(cwd: &str, prefix: &str, out: &mut Vec<(i32, CompletionItem)>) {
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
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let Some((score, idxs)) = fuzzy_match_score(&name, file_prefix) else {
            continue;
        };
        let meta = entry.metadata().ok();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let kind_str = if is_dir { Some("dir") } else { Some("file") };
        let icon = Some(icon_name_for_file(&name, kind_str));
        let label = if is_dir { format!("{name}/") } else { name.clone() };
        let raw_insert = if is_dir {
            format!("{dir_part}{name}/")
        } else {
            format!("{dir_part}{name}")
        };
        let insert = shell_quote_if_needed(&raw_insert);
        let detail = if is_dir {
            "dir".to_owned()
        } else if let Some(m) = meta {
            crate::view::format_value_cell(&blaze_sh::Value::Size(Bytes(
                m.len().min(i64::MAX as u64) as i64,
            )))
        } else {
            "file".to_owned()
        };
        let doc = if dir_part.is_empty() {
            format!("{cwd}/{label}")
        } else {
            raw_insert
        };
        // Slight boost to directories so navigation feels crisp.
        let dir_boost = if is_dir { 5 } else { 0 };
        out.push((
            score + dir_boost,
            CompletionItem {
                label,
                insert,
                detail,
                doc,
                kind: if is_dir { CompletionKind::Dir } else { CompletionKind::File },
                icon,
                match_indices: idxs,
            },
        ));
    }
}

fn word_left_boundary(s: &str, cursor: usize) -> usize {
    let mut i = clamp_char_boundary(s, cursor);
    while i > 0 {
        let p = prev_char_boundary(s, i);
        let ch = s[p..i].chars().next().unwrap_or(' ');
        if !ch.is_whitespace() {
            break;
        }
        i = p;
    }
    while i > 0 {
        let p = prev_char_boundary(s, i);
        let ch = s[p..i].chars().next().unwrap_or(' ');
        if ch.is_whitespace() || is_word_sep(ch) {
            break;
        }
        i = p;
    }
    i
}

fn word_right_boundary(s: &str, cursor: usize) -> usize {
    let mut i = clamp_char_boundary(s, cursor);
    let len = s.len();
    while i < len {
        let n = next_char_boundary(s, i);
        let ch = s[i..n].chars().next().unwrap_or(' ');
        if !ch.is_whitespace() {
            break;
        }
        i = n;
    }
    while i < len {
        let n = next_char_boundary(s, i);
        let ch = s[i..n].chars().next().unwrap_or(' ');
        if ch.is_whitespace() || is_word_sep(ch) {
            break;
        }
        i = n;
    }
    i
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
