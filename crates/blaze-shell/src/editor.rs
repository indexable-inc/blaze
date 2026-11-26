//! Interactive multi-line shell editor: natural macOS editing (`Cmd+Z`/`Cmd+Shift+Z` undo/redo,
//! word/line navigation, selection, click-drag, kill rings), Fish/Nushell inline ghost
//! autosuggestion (`Right` / `Alt+Right`), IntelliJ-style live fuzzy autocomplete with
//! typed icons, column-aware pipeline suggestions, and `Ctrl+R` reverse history search.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use blaze_own::Alloc;
use blaze_sh::builtins::{BUILTIN_SPECS, find_spec};
use blaze_sh::lex::{HighlightSpan, highlight, is_incomplete};
use blaze_sh::{Bytes, EngineState};
use blaze_vim::{
    CursorShape, KeyEvent as VimKeyEvent, Mode as VimMode, Outcome as VimOutcome, ShellObjects,
    TextBuffer, VimMachine, VimOpts,
};

use crate::svg_icons::icon_name_for_file;

struct StdStringBuf<'a>(&'a mut String);

impl TextBuffer for StdStringBuf<'_> {
    #[inline]
    fn text(&self) -> &str {
        self.0.as_str()
    }

    fn replace_range(&mut self, start: usize, end: usize, insert: &str) {
        let s = clamp_char_boundary(self.0, start);
        let e = clamp_char_boundary(self.0, end).max(s);
        self.0.replace_range(s..e, insert);
    }
}

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

/// Scope filter for the Atuin-class `Ctrl+R` command history palette.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HistoryScope {
    #[default]
    Global,
    Host,
    Directory,
    Session,
}

impl HistoryScope {
    pub const fn label(self) -> &'static str {
        match self {
            HistoryScope::Global => "GLOBAL",
            HistoryScope::Host => "HOST",
            HistoryScope::Directory => "DIR",
            HistoryScope::Session => "SESSION",
        }
    }

    pub const fn next(self) -> Self {
        match self {
            HistoryScope::Global => HistoryScope::Host,
            HistoryScope::Host => HistoryScope::Directory,
            HistoryScope::Directory => HistoryScope::Session,
            HistoryScope::Session => HistoryScope::Global,
        }
    }
}

/// Rich structured command history entry (persisted to JSONL across restarts).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryEntry {
    pub command: String,
    pub host: String,
    pub cwd: String,
    pub git_branch: Option<String>,
    pub duration_ms: u64,
    pub exit_code: i32,
    pub timestamp_ns: i64,
    pub in_session: bool,
}

/// One scored fuzzy match inside the `Ctrl+R` Atuin-class history palette.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryMatch {
    pub entry: HistoryEntry,
    pub match_indices: Vec<usize>,
}

/// Atuin-class `Ctrl+R` interactive command history palette state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReverseSearch {
    pub query: String,
    pub scope: HistoryScope,
    pub host: String,
    pub cwd: String,
    pub results: Vec<HistoryMatch>,
    pub selected: usize,
    pub scroll: usize,
    pub total_entries: usize,
    /// Convenience mirror of `results.get(selected).map(|m| m.entry.command.clone())`.
    pub matched_text: Option<String>,
}

/// Semantic action executed from the `Cmd+Shift+P` Command Palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteAction {
    NewTab,
    NewWindow,
    SplitVertical,
    SplitHorizontal,
    ToggleZoom,
    EqualizeSplits,
    ClosePane,
    ForkBranch,
    OpenYazi,
    OpenAgentHub,
    OpenHistory,
    ClearScreen,
    ZoomIn,
    ZoomOut,
    ResetZoom,
}

/// One row inside the searchable `Cmd+Shift+P` Command Palette.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaletteCommand {
    pub title: &'static str,
    pub category: &'static str,
    pub shortcut: &'static str,
    pub action: PaletteAction,
}

/// Stateful monochrome glass Command Palette (`Cmd+Shift+P`) with fuzzy search & smooth scrolling.
#[derive(Clone, Debug)]
pub struct CommandPalette {
    pub query: String,
    pub matches: Vec<(PaletteCommand, Vec<usize>)>,
    pub selected: usize,
    pub scroll: usize,
}

const PALETTE_CATALOG: &[PaletteCommand] = &[
    PaletteCommand { title: "Split Pane Vertically (Right)", category: "Tiling", shortcut: "⌘D", action: PaletteAction::SplitVertical },
    PaletteCommand { title: "Split Pane Horizontally (Down)", category: "Tiling", shortcut: "⌘⇧D", action: PaletteAction::SplitHorizontal },
    PaletteCommand { title: "Toggle Zoom Focused Pane", category: "Tiling", shortcut: "⌘⇧↵", action: PaletteAction::ToggleZoom },
    PaletteCommand { title: "Equalize Split Ratios", category: "Tiling", shortcut: "⌘⌃=", action: PaletteAction::EqualizeSplits },
    PaletteCommand { title: "Close Focused Pane / Tab", category: "Tiling", shortcut: "⌘W", action: PaletteAction::ClosePane },
    PaletteCommand { title: "New Workspace Tab", category: "Workspace", shortcut: "⌘T", action: PaletteAction::NewTab },
    PaletteCommand { title: "New Window (Inherit Directory)", category: "Window", shortcut: "⌘N", action: PaletteAction::NewWindow },
    PaletteCommand { title: "Fork Session Branch (O(1) CoW)", category: "Session", shortcut: "⌘B", action: PaletteAction::ForkBranch },
    PaletteCommand { title: "Open File Browser (y)", category: "Tools", shortcut: "y", action: PaletteAction::OpenYazi },
    PaletteCommand { title: "Open Agent Fleet Hub", category: "Tools", shortcut: "agent", action: PaletteAction::OpenAgentHub },
    PaletteCommand { title: "Search Command History", category: "History", shortcut: "⌃R", action: PaletteAction::OpenHistory },
    PaletteCommand { title: "Clear Screen", category: "View", shortcut: "⌘K", action: PaletteAction::ClearScreen },
    PaletteCommand { title: "Zoom Font In", category: "View", shortcut: "⌘=", action: PaletteAction::ZoomIn },
    PaletteCommand { title: "Zoom Font Out", category: "View", shortcut: "⌘-", action: PaletteAction::ZoomOut },
    PaletteCommand { title: "Reset Font Zoom (100%)", category: "View", shortcut: "⌘0", action: PaletteAction::ResetZoom },
];

pub const PALETTE_VISIBLE_ROWS: usize = 10;
pub const COMPLETION_VISIBLE_ROWS: usize = 9;
pub const HISTORY_VISIBLE_ROWS: usize = 12;

impl CommandPalette {
    pub fn open() -> Self {
        let mut p = Self {
            query: String::new(),
            matches: Vec::new(),
            selected: 0,
            scroll: 0,
        };
        p.refresh();
        p
    }

    pub fn push_char(&mut self, ch: char) {
        self.query.push(ch);
        self.selected = 0;
        self.scroll = 0;
        self.refresh();
    }

    pub fn backspace(&mut self) {
        self.query.pop();
        self.selected = 0;
        self.scroll = 0;
        self.refresh();
    }

    pub fn move_up(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        if self.selected == 0 {
            self.selected = self.matches.len() - 1;
        } else {
            self.selected -= 1;
        }
        self.keep_selected_visible();
    }

    pub fn move_down(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.matches.len();
        self.keep_selected_visible();
    }

    pub fn scroll_by(&mut self, delta_rows: isize) -> bool {
        let max_s = self.matches.len().saturating_sub(PALETTE_VISIBLE_ROWS);
        let next = if delta_rows > 0 {
            self.scroll.saturating_add(delta_rows as usize).min(max_s)
        } else {
            self.scroll.saturating_sub(delta_rows.unsigned_abs())
        };
        let changed = next != self.scroll;
        self.scroll = next;
        changed
    }

    pub fn selected_action(&self) -> Option<PaletteAction> {
        self.matches.get(self.selected).map(|(c, _)| c.action)
    }

    pub fn action_at(&self, idx: usize) -> Option<PaletteAction> {
        self.matches.get(idx).map(|(c, _)| c.action)
    }

    fn keep_selected_visible(&mut self) {
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + PALETTE_VISIBLE_ROWS {
            self.scroll = self.selected + 1 - PALETTE_VISIBLE_ROWS;
        }
    }

    fn refresh(&mut self) {
        let mut scored: Vec<(i32, PaletteCommand, Vec<usize>)> = Vec::new();
        for cmd in PALETTE_CATALOG {
            if let Some((sc, idxs)) = fuzzy_match_score(cmd.title, &self.query) {
                scored.push((sc + 20, cmd.clone(), idxs));
            } else if let Some((sc, _)) = fuzzy_match_score(cmd.category, &self.query) {
                scored.push((sc, cmd.clone(), Vec::new()));
            }
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        self.matches = scored.into_iter().map(|(_, c, i)| (c, i)).collect();
        if self.selected >= self.matches.len() {
            self.selected = 0;
        }
        self.scroll = self.scroll.min(self.matches.len().saturating_sub(PALETTE_VISIBLE_ROWS));
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum EditOp {
    #[default]
    Other,
    InsertWord,
    DeleteBack,
}

/// Multi-line interactive input editor for `blaze-shell`, powered by `blaze-vim`.
pub struct Editor {
    pub text: String,
    /// Byte offset in `text`, always kept on a valid UTF-8 character boundary.
    pub cursor: usize,
    /// Optional mouse/Shift selection anchor byte offset (also a valid UTF-8 boundary).
    pub selection_anchor: Option<usize>,
    /// Submitted command history strings (oldest first, used for Up/Down + Fish ghost).
    pub history: Vec<String>,
    /// Rich structured command history entries (oldest first, persisted across sessions).
    pub history_entries: Vec<HistoryEntry>,
    history_file: Option<PathBuf>,
    history_pos: Option<usize>,
    history_draft: String,
    /// Active `Ctrl+R` reverse search overlay, if open.
    pub reverse_search: Option<ReverseSearch>,
    /// Active IntelliJ-style completion popup candidates.
    pub completions: Vec<CompletionItem>,
    pub completion_selected: usize,
    pub completion_scroll: usize,
    completion_replace_start: usize,
    /// Fish/Nushell-style dimmed inline ghost suffix shown directly after the cursor.
    pub ghost: Option<String>,
    /// Sans-IO Vim modal editing engine (`Normal`/`Insert`/`Visual`/`VisualLine`, undo/redo,
    /// registers, `.` repeat, and pluggable `ShellObjects` including `i|`/`i/`/`i,`).
    vim: VimMachine<'static>,
    last_edit_op: EditOp,
    /// Lazily cached external executable names from `$PATH`.
    path_bins: Option<Vec<String>>,
    /// Command-position abbreviations expanded inline on Space / Enter (like Fish / Nushell).
    pub abbreviations: BTreeMap<String, String>,
    /// Cross-host CRDT frecency store backing zoxide-style `cd` completion & jumping.
    pub zoxide: crate::zoxide::FrecencyStore,
    /// Local machine short hostname (cached at startup).
    pub local_host: String,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
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
    ("c", "cd"),
    ("cl", "clear"),
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
    ("agent", "interactive multi-server agent fleet (or `agent \"prompt\"`)"),
    ("y", "Yazi-class 3-column GPU file manager with live previews"),
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
        let mut ed = Self {
            text: String::new(),
            cursor: 0,
            selection_anchor: None,
            history: Vec::new(),
            history_entries: Vec::new(),
            history_file: None,
            history_pos: None,
            history_draft: String::new(),
            reverse_search: None,
            completions: Vec::new(),
            completion_selected: 0,
            completion_scroll: 0,
            completion_replace_start: 0,
            ghost: None,
            vim: VimMachine::new(
                VimOpts {
                    initial_mode: VimMode::Insert,
                    max_undo: MAX_UNDO_DEPTH,
                    max_count: 10_000,
                },
                Alloc::global(),
            ),
            last_edit_op: EditOp::Other,
            path_bins: None,
            abbreviations: BTreeMap::new(),
            zoxide: crate::zoxide::FrecencyStore::new(),
            local_host: crate::zoxide::detect_local_hostname(),
        };
        ed.load_default_abbreviations();
        ed
    }

    #[inline]
    pub fn vim_mode(&self) -> VimMode {
        self.vim.mode()
    }

    #[inline]
    pub fn vim_cursor_shape(&self) -> CursorShape {
        self.vim.cursor_shape()
    }

    pub fn vim_pending(&self) -> String {
        let mut s = String::new();
        let _ = self.vim.format_pending(&mut s);
        s
    }

    pub fn vim_register_text(&self) -> &str {
        self.vim.register().text.as_str()
    }

    pub fn enter_vim_insert(&mut self) {
        self.vim.set_mode(&self.text, VimMode::Insert);
        self.vim.set_cursor(&self.text, self.cursor);
    }

    /// Feed a normalized keystroke into `blaze-vim` with `ShellObjects` (`i|`/`i/`/`i,` + standard Vim).
    pub fn feed_vim_key(&mut self, ev: VimKeyEvent) -> VimOutcome {
        self.clamp_cursor();
        let prev_mode = self.vim.mode();
        self.vim.set_cursor(&self.text, self.cursor);
        let outcome = self.vim.feed(ev, &mut StdStringBuf(&mut self.text), &ShellObjects);
        self.cursor = clamp_char_boundary(&self.text, self.vim.cursor());
        if !matches!(self.vim.mode(), VimMode::Insert) {
            self.selection_anchor = None;
            self.ghost = None;
            self.dismiss_completions();
        }
        // Coalesce subsequent typing into a single atomic undo step after Vim change (`c`/`s`/`C`/`S`).
        self.last_edit_op = if prev_mode != VimMode::Insert
            && self.vim.mode() == VimMode::Insert
            && matches!(outcome, VimOutcome::Handled { mutated: true, .. })
        {
            EditOp::InsertWord
        } else {
            EditOp::Other
        };
        outcome
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
        let coalesce = op != EditOp::Other && op == self.last_edit_op;
        if !coalesce {
            self.vim.set_cursor(&self.text, self.cursor);
            self.vim.checkpoint_undo(&self.text);
        }
        self.last_edit_op = op;
    }

    /// `Cmd+Z` / Vim `u`: restore previous editor state from the unified `blaze-vim` undo stack.
    pub fn undo(&mut self) -> bool {
        self.vim.set_cursor(&self.text, self.cursor);
        let ok = self.vim.undo(&mut StdStringBuf(&mut self.text));
        if ok {
            self.cursor = clamp_char_boundary(&self.text, self.vim.cursor());
            self.selection_anchor = None;
            self.last_edit_op = EditOp::Other;
            self.history_pos = None;
            self.dismiss_completions();
            self.ghost = None;
        }
        ok
    }

    /// `Cmd+Shift+Z` / Vim `Ctrl+R`: reapply an undone edit.
    pub fn redo(&mut self) -> bool {
        self.vim.set_cursor(&self.text, self.cursor);
        let ok = self.vim.redo(&mut StdStringBuf(&mut self.text));
        if ok {
            self.cursor = clamp_char_boundary(&self.text, self.vim.cursor());
            self.selection_anchor = None;
            self.last_edit_op = EditOp::Other;
            self.history_pos = None;
            self.dismiss_completions();
            self.ghost = None;
        }
        ok
    }

    pub fn selection_range(&self) -> Option<(usize, usize)> {
        if let Some(vis) = self.vim.visual_selection(&self.text) {
            if !vis.is_empty() {
                return Some((vis.start, vis.end));
            }
        }
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
        self.vim.reset_for_new_buffer();
        self.vim.set_cursor(&self.text, self.cursor);
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

    /// Interactive keyboard typing with IntelliJ-style auto-pairing (`{}`, `()`, `[]`, `""`, `''`),
    /// selection wrapping (`{selected}`), and closing-delimiter overtype.
    pub fn insert_typed(&mut self, s: &str) {
        let mut chars = s.chars();
        let Some(ch) = chars.next() else { return };
        if chars.next().is_some() {
            self.insert_str(s);
            return;
        }

        let pair_close = |open: char| -> Option<char> {
            match open {
                '{' => Some('}'),
                '(' => Some(')'),
                '[' => Some(']'),
                '"' => Some('"'),
                '\'' => Some('\''),
                _ => None,
            }
        };

        // 1. Wrap active selection when typing an opening bracket or quote.
        if let Some((a, b)) = self.selection_range()
            && let Some(close) = pair_close(ch)
        {
            self.push_undo(EditOp::Other);
            let mut open_buf = [0u8; 4];
            let mut close_buf = [0u8; 4];
            let os = ch.encode_utf8(&mut open_buf);
            let cs = close.encode_utf8(&mut close_buf);
            self.text.insert_str(b, cs);
            self.text.insert_str(a, os);
            self.selection_anchor = Some(a + os.len());
            self.cursor = b + os.len();
            self.history_pos = None;
            self.dismiss_completions();
            return;
        }

        self.clamp_cursor();
        let next_ch = self.text[self.cursor..].chars().next();
        let prev_ch = self.text[..self.cursor].chars().next_back();

        // 2. Overtype closing delimiter if cursor is already right before it.
        if matches!(ch, '}' | ')' | ']' | '"' | '\'') && next_ch == Some(ch) {
            self.cursor += ch.len_utf8();
            self.selection_anchor = None;
            self.dismiss_completions();
            return;
        }

        // 3. Auto-pair opening brackets and balanced quotes when followed by boundary/delimiter.
        if let Some(close) = pair_close(ch) {
            let next_ok = next_ch.is_none_or(|c| {
                c.is_whitespace() || matches!(c, ')' | ']' | '}' | ',' | ';' | '|' | ':')
            });
            let is_quote = ch == '"' || ch == '\'';
            let prev_ok = !is_quote
                || prev_ch.is_none_or(|c| {
                    c.is_whitespace() || matches!(c, '(' | '[' | '{' | '=' | ',' | ':' | '|')
                });
            let quote_balanced = !is_quote || self.text.chars().filter(|&c| c == ch).count() % 2 == 0;
            if next_ok && prev_ok && quote_balanced {
                self.push_undo(EditOp::Other);
                let mut pair = String::with_capacity(2);
                pair.push(ch);
                pair.push(close);
                self.text.insert_str(self.cursor, &pair);
                self.cursor += ch.len_utf8();
                self.selection_anchor = None;
                self.history_pos = None;
                self.dismiss_completions();
                return;
            }
        }

        self.insert_str(s);
    }

    /// IntelliJ-style Tab-out: when cursor sits right before a closing delimiter (`}`, `)`, `]`,
    /// `"`, `'`), jump cursor out past it and return `true`.
    pub fn tab_out(&mut self) -> bool {
        if self.selection_range().is_some() {
            return false;
        }
        self.clamp_cursor();
        if let Some(ch) = self.text[self.cursor..].chars().next()
            && matches!(ch, '}' | ')' | ']' | '"' | '\'')
        {
            self.cursor += ch.len_utf8();
            self.last_edit_op = EditOp::Other;
            return true;
        }
        false
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
        let prev_ch = self.text[prev..self.cursor].chars().next();
        let next_ch = self.text[self.cursor..].chars().next();
        let delete_end = if matches!(
            (prev_ch, next_ch),
            (Some('{'), Some('}'))
                | (Some('('), Some(')'))
                | (Some('['), Some(']'))
                | (Some('"'), Some('"'))
                | (Some('\''), Some('\''))
        ) {
            next_char_boundary(&self.text, self.cursor)
        } else {
            self.cursor
        };
        self.text.drain(prev..delete_end);
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
            self.sync_completion_scroll();
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
            self.sync_completion_scroll();
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
        // Keep `history_entries` synced for unit tests / quick callers that don't pass rich metadata.
        if self
            .history_entries
            .last()
            .map(|e| e.command.as_str())
            != Some(trimmed)
        {
            self.history_entries.push(HistoryEntry {
                command: trimmed.to_owned(),
                host: self.local_host.clone(),
                cwd: String::new(),
                git_branch: None,
                duration_ms: 0,
                exit_code: 0,
                timestamp_ns: 0,
                in_session: true,
            });
        }
    }

    /// Record a rich command execution in history and append to persistent JSONL store.
    pub fn record_history_entry(&mut self, entry: HistoryEntry) {
        let trimmed = entry.command.trim();
        if trimmed.is_empty() {
            return;
        }
        if self.history.last().map(|s| s.as_str()) != Some(trimmed) {
            self.history.push(trimmed.to_owned());
        }
        // Replace placeholder entry pushed by `take_submitted` if it matches.
        if let Some(last) = self.history_entries.last_mut()
            && last.command == trimmed
            && last.timestamp_ns == 0
        {
            *last = entry.clone();
        } else {
            self.history_entries.push(entry.clone());
        }
        const MAX_HISTORY_ENTRIES: usize = 10_000;
        if self.history_entries.len() > MAX_HISTORY_ENTRIES {
            let drop_n = self.history_entries.len() - MAX_HISTORY_ENTRIES;
            self.history_entries.drain(0..drop_n);
        }
        if let Some(path) = &self.history_file {
            append_history_jsonl(path, &entry);
        }
    }

    /// Load persistent command history from `<home>/.local/share/blaze-shell/history.jsonl`
    /// and cross-host zoxide CRDT frecency from `zoxide.jsonl`.
    pub fn load_persistent_history(&mut self, home: &str) {
        let dir = PathBuf::from(home).join(".local/share/blaze-shell");
        let path = dir.join("history.jsonl");
        let _ = fs::create_dir_all(&dir);
        self.zoxide.load(home);
        if let Ok(raw) = fs::read_to_string(&path) {
            for line in raw.lines().rev().take(4000).collect::<Vec<_>>().into_iter().rev() {
                if let Some(mut entry) = parse_history_jsonl_line(line) {
                    if entry.host.is_empty() {
                        entry.host = self.local_host.clone();
                    }
                    if !entry.cwd.is_empty() {
                        self.zoxide.join_entry(crate::zoxide::FrecencyEntry {
                            host: entry.host.clone(),
                            path: entry.cwd.clone(),
                            visits: 1,
                            last_ns: entry.timestamp_ns,
                        });
                    }
                    if self.history.last().map(|s| s.as_str()) != Some(entry.command.as_str()) {
                        self.history.push(entry.command.clone());
                    }
                    self.history_entries.push(entry);
                }
            }
        }
        self.history_file = Some(path);
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
        self.vim.reset_for_new_buffer();
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
    // Ctrl+R Atuin-class fuzzy command history palette
    // -------------------------------------------------------------------------

    pub fn start_reverse_search(&mut self) {
        self.start_reverse_search_in("");
    }

    pub fn start_reverse_search_in(&mut self, cwd: &str) {
        let host = self.local_host.clone();
        self.start_reverse_search_for(&host, cwd);
    }

    pub fn start_reverse_search_for(&mut self, host: &str, cwd: &str) {
        if let Some(rs) = &mut self.reverse_search {
            // Pressing Ctrl+R while already open cycles scope: GLOBAL -> HOST -> DIR -> SESSION.
            rs.scope = rs.scope.next();
            rs.selected = 0;
            update_reverse_search(rs, &self.history_entries);
            return;
        }
        self.dismiss_completions();
        let mut rs = ReverseSearch {
            query: String::new(),
            scope: HistoryScope::Global,
            host: host.to_owned(),
            cwd: cwd.to_owned(),
            results: Vec::new(),
            selected: 0,
            scroll: 0,
            total_entries: self.history_entries.len(),
            matched_text: None,
        };
        update_reverse_search(&mut rs, &self.history_entries);
        self.reverse_search = Some(rs);
    }

    pub fn reverse_search_cycle_scope(&mut self) {
        if let Some(rs) = &mut self.reverse_search {
            rs.scope = rs.scope.next();
            rs.selected = 0;
            rs.scroll = 0;
            update_reverse_search(rs, &self.history_entries);
        }
    }

    pub fn reverse_search_push(&mut self, ch: char) {
        if let Some(rs) = &mut self.reverse_search {
            rs.query.push(ch);
            rs.selected = 0;
            rs.scroll = 0;
            update_reverse_search(rs, &self.history_entries);
        }
    }

    pub fn reverse_search_backspace(&mut self) {
        if let Some(rs) = &mut self.reverse_search {
            rs.query.pop();
            rs.selected = 0;
            rs.scroll = 0;
            update_reverse_search(rs, &self.history_entries);
        }
    }

    pub fn reverse_search_up(&mut self) {
        if let Some(rs) = &mut self.reverse_search
            && !rs.results.is_empty()
        {
            if rs.selected + 1 < rs.results.len() {
                rs.selected += 1;
            } else {
                rs.selected = 0;
            }
            if rs.selected < rs.scroll {
                rs.scroll = rs.selected;
            } else if rs.selected >= rs.scroll + HISTORY_VISIBLE_ROWS {
                rs.scroll = rs.selected + 1 - HISTORY_VISIBLE_ROWS;
            }
            rs.matched_text = rs.results.get(rs.selected).map(|m| m.entry.command.clone());
        }
    }

    pub fn reverse_search_down(&mut self) {
        if let Some(rs) = &mut self.reverse_search
            && !rs.results.is_empty()
        {
            if rs.selected == 0 {
                rs.selected = rs.results.len() - 1;
            } else {
                rs.selected -= 1;
            }
            if rs.selected < rs.scroll {
                rs.scroll = rs.selected;
            } else if rs.selected >= rs.scroll + HISTORY_VISIBLE_ROWS {
                rs.scroll = rs.selected + 1 - HISTORY_VISIBLE_ROWS;
            }
            rs.matched_text = rs.results.get(rs.selected).map(|m| m.entry.command.clone());
        }
    }

    /// Scroll the `Ctrl+R` history list directly via trackpad/mouse wheel (`delta_rows > 0` scrolls older).
    pub fn reverse_search_scroll_by(&mut self, delta_rows: isize) -> bool {
        let Some(rs) = &mut self.reverse_search else {
            return false;
        };
        let max_s = rs.results.len().saturating_sub(HISTORY_VISIBLE_ROWS);
        let next = if delta_rows > 0 {
            rs.scroll.saturating_add(delta_rows as usize).min(max_s)
        } else {
            rs.scroll.saturating_sub(delta_rows.unsigned_abs())
        };
        let changed = next != rs.scroll;
        rs.scroll = next;
        changed
    }

    pub fn reverse_search_next(&mut self) {
        self.reverse_search_up();
    }

    pub fn accept_reverse_search(&mut self) -> bool {
        let Some(rs) = self.reverse_search.take() else {
            return false;
        };
        if let Some(matched) = rs.matched_text {
            self.set_text(matched);
            self.enter_vim_insert();
            true
        } else {
            false
        }
    }

    /// Accept row `idx` of the history palette into the prompt for editing (or return it to run).
    pub fn take_reverse_search_index(&mut self, idx: usize) -> Option<String> {
        let rs = self.reverse_search.take()?;
        rs.results.get(idx).map(|m| m.entry.command.clone())
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
        self.completion_scroll = 0;
    }

    fn sync_completion_scroll(&mut self) {
        if self.completion_selected < self.completion_scroll {
            self.completion_scroll = self.completion_selected;
        } else if self.completion_selected >= self.completion_scroll + COMPLETION_VISIBLE_ROWS {
            self.completion_scroll = self.completion_selected + 1 - COMPLETION_VISIBLE_ROWS;
        }
    }

    /// Scroll the open autocomplete popup directly via trackpad/mouse wheel!
    pub fn completion_scroll_by(&mut self, delta_rows: isize) -> bool {
        if self.completions.is_empty() {
            return false;
        }
        let max_s = self.completions.len().saturating_sub(COMPLETION_VISIBLE_ROWS);
        let next = if delta_rows > 0 {
            self.completion_scroll.saturating_add(delta_rows as usize).min(max_s)
        } else {
            self.completion_scroll.saturating_sub(delta_rows.unsigned_abs())
        };
        let changed = next != self.completion_scroll;
        self.completion_scroll = next;
        changed
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

        // 1. Instant <0.05ms ghost prediction when cursor is at end of single-line buffer:
        //    (a) Exact history prefix match (newest first),
        //    (b) History Markov bigram transition from the previous command,
        //    (c) Context-aware command/SSH/zoxide/git/blaze zero-history priors!
        let at_eol = self.cursor == self.text.len() && !self.text.contains('\n');
        if at_eol {
            for hist in self.history.iter().rev() {
                if hist.len() > self.text.len() && hist.starts_with(&self.text) {
                    self.ghost = Some(hist[self.text.len()..].to_owned());
                    break;
                }
            }
            if self.ghost.is_none() {
                self.ghost = self.predict_zero_latency_ghost(state);
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
            self.completion_scroll = 0;
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
                self.sync_completion_scroll();
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
        self.completion_scroll = 0;

        if self.completions.len() == 1 {
            self.apply_selected_completion();
        }
    }

    /// Zero-latency (<0.05ms) context-aware ghost predictor that recommends smart completions
    /// even for commands the user has never typed before:
    /// 1. Learned Markov `(prev_cmd -> next_cmd)` transitions from `history_entries`,
    /// 2. Live `blaze-ssh` hosts after `ssh `, live `zoxide` CRDT top dir after `cd `,
    /// 3. High-signal command/flag/subcommand priors (`git`, `blaze`, `agent`, `ls`, `rg`, `y`).
    fn predict_zero_latency_ghost(&self, state: &EngineState<'_>) -> Option<String> {
        let cur = self.text.as_str();
        if cur.is_empty() {
            return None;
        }

        // 1. Learned Markov bigram: what command most often followed our last executed command?
        if let Some(prev) = self.history.last() {
            let mut counts: HashMap<&str, usize> = HashMap::new();
            for pair in self.history.windows(2) {
                if pair[0] == *prev && pair[1].len() > cur.len() && pair[1].starts_with(cur) {
                    *counts.entry(pair[1].as_str()).or_insert(0) += 1;
                }
            }
            if let Some((best, _)) = counts.into_iter().max_by_key(|&(_, n)| n) {
                return Some(best[cur.len()..].to_owned());
            }
        }

        // 2. Live SSH host prediction when typing `ssh <partial>`!
        if let Some(rest) = cur.strip_prefix("ssh ") {
            let home = state.get_env("HOME").unwrap_or("");
            for (alias, _, _) in collect_ssh_hosts(home) {
                if alias.len() > rest.len() && alias.starts_with(rest) {
                    return Some(alias[rest.len()..].to_owned());
                }
            }
        }

        // 3. Live Zoxide CRDT top directory prediction when typing `cd <partial>`!
        if let Some(rest) = cur.strip_prefix("cd ") {
            let now_ns = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0);
            if let Some(top) = self.zoxide.query(rest, &self.local_host, now_ns, 1).into_iter().next() {
                let target = if top.is_local {
                    top.path
                } else {
                    format!("{}:{}", top.host, top.path)
                };
                if target.len() > rest.len() && target.starts_with(rest) {
                    return Some(target[rest.len()..].to_owned());
                }
            }
        }

        // 4. Built-in zero-history command & workflow priors (instant, deterministic, zero jitter).
        const PRIORS: &[&str] = &[
            "git status",
            "git diff",
            "git commit -m \"\"",
            "git switch ",
            "git log --oneline -n 20",
            "git push",
            "git pull --rebase",
            "blaze check",
            "blaze test",
            "blaze run",
            "blaze export",
            "agent \"\"",
            "ls -l | sort-by size -r",
            "ps | sort-by cpu -r | first 10",
            "rg \"\"",
            "y ",
            "open Cargo.toml",
            "ssh ",
        ];
        for &cand in PRIORS {
            if cand.len() > cur.len() && cand.starts_with(cur) {
                return Some(cand[cur.len()..].to_owned());
            }
        }
        None
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
            // Smart command-aware argument completions (`ssh`, `git`, `blaze`, `cd`, table columns).
            let stage_start = before.rfind(['|', ';', '\n']).map_or(0, |p| p + 1);
            let stage_prefix = before[stage_start..token_start].trim();
            let stage_words: Vec<&str> = stage_prefix.split_whitespace().collect();
            let cmd0 = stage_words.first().copied().unwrap_or("");
            let cmd_base = cmd0.trim_start_matches('^').rsplit('/').next().unwrap_or(cmd0);
            let subcmd = stage_words.get(1).copied();

            let mut handled_specialized = false;

            if matches!(cmd_base, "ssh" | "sftp" | "mosh")
                || (matches!(cmd_base, "scp" | "rsync")
                    && !prefix.starts_with('/')
                    && !prefix.starts_with("./")
                    && !prefix.starts_with("../")
                    && !prefix.starts_with('~'))
            {
                let home = state.get_env("HOME").unwrap_or("");
                let (user_prefix, host_prefix) = match prefix.split_once('@') {
                    Some((u, h)) => (Some(u), h),
                    None => (None, prefix),
                };
                for (alias, detail, doc) in collect_ssh_hosts(home) {
                    if let Some((score, mut idxs)) = fuzzy_match_score(&alias, host_prefix) {
                        let (label, insert) = if let Some(u) = user_prefix {
                            for i in &mut idxs {
                                *i += u.len() + 1;
                            }
                            (format!("{u}@{alias}"), format!("{u}@{alias}"))
                        } else {
                            (alias.clone(), alias.clone())
                        };
                        scored.push((
                            score + 70,
                            CompletionItem {
                                label,
                                insert,
                                detail,
                                doc,
                                kind: CompletionKind::External,
                                icon: Some("shell_dark"),
                                match_indices: idxs,
                            },
                        ));
                    }
                }
                if matches!(cmd_base, "ssh" | "sftp" | "mosh") && !scored.is_empty() {
                    handled_specialized = true;
                }
            } else if cmd_base == "git" {
                if subcmd.is_none() {
                    for &(sub, desc) in GIT_SUBCOMMANDS {
                        if let Some((score, idxs)) = fuzzy_match_score(sub, prefix) {
                            scored.push((
                                score + 65,
                                CompletionItem {
                                    label: sub.to_owned(),
                                    insert: sub.to_owned(),
                                    detail: "git".to_owned(),
                                    doc: desc.to_owned(),
                                    kind: CompletionKind::Builtin,
                                    icon: None,
                                    match_indices: idxs,
                                },
                            ));
                        }
                    }
                    handled_specialized = true;
                } else if matches!(
                    subcmd,
                    Some("switch" | "checkout" | "co" | "merge" | "rebase" | "branch" | "diff" | "log" | "push" | "reset")
                ) {
                    let branches = crate::prompt::read_local_branches_for_cwd(state.cwd.as_ref(), 32);
                    for br in branches {
                        if let Some((score, idxs)) = fuzzy_match_score(&br, prefix) {
                            scored.push((
                                score + 60,
                                CompletionItem {
                                    label: br.clone(),
                                    insert: br.clone(),
                                    detail: "branch".to_owned(),
                                    doc: format!("local git branch `{br}`"),
                                    kind: CompletionKind::Keyword,
                                    icon: None,
                                    match_indices: idxs,
                                },
                            ));
                        }
                    }
                    if matches!(subcmd, Some("switch" | "merge" | "rebase" | "branch")) && !scored.is_empty() {
                        handled_specialized = true;
                    }
                }
            } else if cmd_base == "blaze" && subcmd.is_none() {
                for &(sub, desc) in BLAZE_SUBCOMMANDS {
                    if let Some((score, idxs)) = fuzzy_match_score(sub, prefix) {
                        scored.push((
                            score + 65,
                            CompletionItem {
                                label: sub.to_owned(),
                                insert: sub.to_owned(),
                                detail: "blaze".to_owned(),
                                doc: desc.to_owned(),
                                kind: CompletionKind::Builtin,
                                icon: Some("rust_dark"),
                                match_indices: idxs,
                            },
                        ));
                    }
                }
                handled_specialized = true;
            }

            // Column-aware completion for table pipeline operators (`where`, `sort-by`, `select`, `get`, ...).
            if !handled_specialized && expects_column_arg(before) {
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
            // Filesystem completion with macOS folder + IntelliJ New UI dark SVG icons (`cd`/`z` = dirs + cross-host zoxide CRDT).
            if !handled_specialized {
                if matches!(cmd_base, "cd" | "z") {
                    let home = state.get_env("HOME").unwrap_or("");
                    let now_ns = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos() as i64)
                        .unwrap_or(0);
                    for zm in self.zoxide.query(prefix, &self.local_host, now_ns, 20) {
                        let short = crate::prompt::abbreviate_home(&zm.path, home);
                        let (label, insert, detail, doc) = if zm.is_local {
                            (
                                short.clone(),
                                shell_quote_if_needed(&zm.path),
                                format!("z · {:.0}", zm.score),
                                format!("zoxide ({}) · {}", zm.host, zm.path),
                            )
                        } else {
                            (
                                format!("{}:{}", zm.host, short),
                                shell_quote_if_needed(&format!("{}:{}", zm.host, zm.path)),
                                format!("host · {}", zm.host),
                                format!("remote zoxide on {} · {}", zm.host, zm.path),
                            )
                        };
                        let match_indices = fuzzy_match_score(&label, prefix)
                            .map(|(_, idxs)| idxs)
                            .unwrap_or_default();
                        scored.push((
                            (zm.score as i32).clamp(15, 160),
                            CompletionItem {
                                label,
                                insert,
                                detail,
                                doc,
                                kind: CompletionKind::Dir,
                                icon: Some("folder_macos"),
                                match_indices,
                            },
                        ));
                    }
                }
                collect_path_completions(state.cwd.as_ref(), prefix, matches!(cmd_base, "cd" | "z"), &mut scored);
            }
        }

        // Highest match score first, then alphabetical by label.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.label.cmp(&b.1.label)));

        // Deduplicate by label (keeping highest-ranked entry).
        let mut seen_labels: HashSet<String> = HashSet::new();
        let mut items: Vec<CompletionItem> = Vec::new();
        for (_, item) in scored {
            if seen_labels.insert(item.label.clone()) {
                items.push(item);
                if items.len() >= 64 {
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

fn update_reverse_search(rs: &mut ReverseSearch, entries: &[HistoryEntry]) {
    rs.total_entries = entries.len();
    let mut seen: HashSet<String> = HashSet::new();
    let mut scored: Vec<(i32, usize, HistoryMatch)> = Vec::new();

    // Walk newest-first so recency wins ties and deduplication keeps the latest run.
    for (rev_rank, entry) in entries.iter().rev().enumerate() {
        match rs.scope {
            HistoryScope::Global => {}
            HistoryScope::Host => {
                if !rs.host.is_empty() && !entry.host.is_empty() && entry.host != rs.host {
                    continue;
                }
            }
            HistoryScope::Directory => {
                if !rs.cwd.is_empty() && entry.cwd != rs.cwd {
                    continue;
                }
            }
            HistoryScope::Session => {
                if !entry.in_session {
                    continue;
                }
            }
        }
        if !seen.insert(format!("{}\0{}", entry.host, entry.command)) {
            continue;
        }
        let (mut score, idxs) = if let Some(m) = fuzzy_match_score(&entry.command, &rs.query) {
            m
        } else if !rs.query.is_empty() && fuzzy_match_score(&entry.host, &rs.query).is_some() {
            (20, Vec::new())
        } else {
            continue;
        };
        // Boost same-directory commands in Global search and successful runs.
        if !rs.cwd.is_empty() && entry.cwd == rs.cwd {
            score += 25;
        }
        if entry.exit_code == 0 {
            score += 6;
        }
        // Mild recency preference so recent commands float near the bottom/top cleanly.
        let recency_bonus = 40i32.saturating_sub((rev_rank / 4) as i32).max(0);
        scored.push((
            score + recency_bonus,
            rev_rank,
            HistoryMatch { entry: entry.clone(), match_indices: idxs },
        ));
    }

    // When query is empty, preserve pure reverse-chronological order; otherwise rank by score then recency.
    if !rs.query.is_empty() {
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    }
    scored.truncate(60);

    rs.results = scored.into_iter().map(|(_, _, m)| m).collect();
    if rs.selected >= rs.results.len() {
        rs.selected = 0;
    }
    rs.matched_text = rs.results.get(rs.selected).map(|m| m.entry.command.clone());
}

fn append_history_jsonl(path: &Path, e: &HistoryEntry) {
    use std::io::Write as _;
    let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let esc = |s: &str| {
        let mut out = String::with_capacity(s.len());
        for ch in s.chars() {
            match ch {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c => out.push(c),
            }
        }
        out
    };
    let branch_json = match &e.git_branch {
        Some(b) => format!("\"{}\"", esc(b)),
        None => "null".to_owned(),
    };
    let _ = writeln!(
        f,
        "{{\"cmd\":\"{}\",\"host\":\"{}\",\"cwd\":\"{}\",\"branch\":{},\"dur\":{},\"exit\":{},\"ts\":{}}}",
        esc(&e.command),
        esc(&e.host),
        esc(&e.cwd),
        branch_json,
        e.duration_ms,
        e.exit_code,
        e.timestamp_ns,
    );
}

fn parse_history_jsonl_line(line: &str) -> Option<HistoryEntry> {
    let alloc = Alloc::global();
    let val = blaze_json::read_in(line.as_bytes(), blaze_json::Limits::SHIPPED, alloc).ok()?;
    let blaze_data::Value::Map(ref map) = val else {
        return None;
    };
    let mut cmd = String::new();
    let mut host = String::new();
    let mut cwd = String::new();
    let mut git_branch = None;
    let mut duration_ms = 0u64;
    let mut exit_code = 0i32;
    let mut timestamp_ns = 0i64;
    for (k, v) in map.iter() {
        match (k, v) {
            ("cmd", blaze_data::Value::Text(s)) => cmd = s.as_str().to_owned(),
            ("host", blaze_data::Value::Text(s)) => host = s.as_str().to_owned(),
            ("cwd", blaze_data::Value::Text(s)) => cwd = s.as_str().to_owned(),
            ("branch", blaze_data::Value::Text(s)) => git_branch = Some(s.as_str().to_owned()),
            ("dur", blaze_data::Value::Int(n)) => duration_ms = n.as_i64().unwrap_or(0).max(0) as u64,
            ("exit", blaze_data::Value::Int(n)) => exit_code = n.as_i64().unwrap_or(0) as i32,
            ("ts", blaze_data::Value::Int(n)) => timestamp_ns = n.as_i64().unwrap_or(0),
            _ => {}
        }
    }
    if cmd.is_empty() {
        return None;
    }
    Some(HistoryEntry {
        command: cmd,
        host,
        cwd,
        git_branch,
        duration_ms,
        exit_code,
        timestamp_ns,
        in_session: false,
    })
}

fn active_stage_command(before_cursor: &str) -> Option<&str> {
    let stage_start = before_cursor
        .rfind(['|', ';', '\n'])
        .map_or(0, |p| p + 1);
    before_cursor[stage_start..].split_whitespace().next()
}

const GIT_SUBCOMMANDS: &[(&str, &str)] = &[
    ("status", "Show working tree status"),
    ("diff", "Show changes between commits or working tree"),
    ("log", "Show commit logs"),
    ("switch", "Switch branches"),
    ("checkout", "Switch branches or restore working tree files"),
    ("branch", "List, create, or delete branches"),
    ("commit", "Record changes to the repository"),
    ("add", "Add file contents to the index"),
    ("push", "Update remote refs along with associated objects"),
    ("pull", "Fetch from and integrate with another repository"),
    ("fetch", "Download objects and refs from another repository"),
    ("rebase", "Reapply commits on top of another base tip"),
    ("stash", "Stash changes in a dirty working directory"),
    ("merge", "Join two or more development histories together"),
    ("restore", "Restore working tree files"),
    ("reset", "Reset current HEAD to the specified state"),
    ("show", "Show various types of objects"),
    ("cherry-pick", "Apply changes introduced by existing commits"),
    ("worktree", "Manage multiple working trees"),
];

const BLAZE_SUBCOMMANDS: &[(&str, &str)] = &[
    ("check", "Check and lint a workspace incrementally"),
    ("run", "Compile what is reached and run a binary"),
    ("test", "Run workspace tests incrementally"),
    ("export", "Export a standalone PGO-optimized native binary"),
    ("ci", "Run CI checks with structured reporting"),
    ("inspect", "Inspect lowered MIR/types for a file or function"),
    ("why", "Explain why a file or item was rechecked"),
    ("explain", "Explain a diagnostic or lint code"),
    ("clean", "Remove checkout state (and --store for home store)"),
    ("gc", "Compact store and evict stale records"),
    ("peer", "Serve crate records to LAN peers"),
    ("toolchain", "Print the pinned Rust toolchain"),
    ("warm-toolchain", "Pre-warm toolchain records into the store"),
    ("version", "Print blaze and toolchain versions"),
    ("help", "Show blaze CLI help"),
];

/// Load SSH hosts via our sans-IO `blaze-ssh` parser (`~/.ssh/config` + `Include`s + `known_hosts`).
fn collect_ssh_hosts(home: &str) -> Vec<(String, String, String)> {
    let mut out: Vec<(String, String, String)> = Vec::new();
    if home.is_empty() {
        return out;
    }
    let ssh_dir = PathBuf::from(home).join(".ssh");
    let alloc = Alloc::global();
    let mut seen: HashSet<String> = HashSet::new();

    if let Ok(root_text) = fs::read_to_string(ssh_dir.join("config")) {
        let mut loader =
            blaze_ssh::ConfigLoader::new(&root_text, blaze_ssh::ConfigLimits::SHIPPED, alloc);
        for depth in 2..=blaze_ssh::ConfigLimits::SHIPPED.max_include_depth {
            match loader.poll() {
                blaze_ssh::LoadStep::NeedInclude(pat) => {
                    let texts = read_ssh_include_pattern(&ssh_dir, home, pat.as_str());
                    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
                    loader.feed_files(&refs, depth);
                }
                blaze_ssh::LoadStep::Done(cfg) => {
                    for h in cfg.list_hosts(128, alloc).iter() {
                        let alias = h.alias.as_str().to_owned();
                        if !seen.insert(alias.clone()) {
                            continue;
                        }
                        let target = match (&h.user, &h.hostname, h.port) {
                            (Some(u), Some(hn), Some(p)) if p != 22 => {
                                format!("{}@{}:{p}", u.as_str(), hn.as_str())
                            }
                            (Some(u), Some(hn), _) => format!("{}@{}", u.as_str(), hn.as_str()),
                            (None, Some(hn), Some(p)) if p != 22 => {
                                format!("{}:{p}", hn.as_str())
                            }
                            (None, Some(hn), _) => hn.as_str().to_owned(),
                            (Some(u), None, _) => format!("{}@{alias}", u.as_str()),
                            _ => "ssh host".to_owned(),
                        };
                        let doc = if let Some(pj) = &h.proxy_jump {
                            format!("~/.ssh/config · {target} (via {})", pj.as_str())
                        } else {
                            format!("~/.ssh/config · {target}")
                        };
                        out.push((alias, target, doc));
                    }
                    break;
                }
            }
        }
    }

    if let Ok(kh_text) = fs::read_to_string(ssh_dir.join("known_hosts")) {
        for kh in blaze_ssh::parse_known_hosts(&kh_text, 128, alloc).iter() {
            let host = kh.host.as_str().to_owned();
            if seen.insert(host.clone()) {
                let detail = match kh.port {
                    Some(p) => format!("known:{p}"),
                    None => "known_hosts".to_owned(),
                };
                out.push((host.clone(), detail, format!("~/.ssh/known_hosts · {host}")));
            }
        }
    }
    out
}

fn read_ssh_include_pattern(ssh_dir: &Path, home: &str, raw_pat: &str) -> Vec<String> {
    let expanded = if let Some(rest) = raw_pat.strip_prefix("~/") {
        PathBuf::from(home).join(rest)
    } else if raw_pat.starts_with('/') {
        PathBuf::from(raw_pat)
    } else {
        ssh_dir.join(raw_pat)
    };
    let s = expanded.to_string_lossy();
    if !s.contains('*') && !s.contains('?') {
        return fs::read_to_string(&expanded).into_iter().collect();
    }
    let Some(parent) = expanded.parent() else {
        return Vec::new();
    };
    let Some(file_pat) = expanded.file_name().and_then(|n| n.to_str()) else {
        return Vec::new();
    };
    let Ok(rd) = fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if blaze_ssh::glob_match(file_pat, &name) && e.file_type().is_ok_and(|t| t.is_file()) {
                Some(e.path())
            } else {
                None
            }
        })
        .collect();
    paths.sort();
    paths
        .into_iter()
        .take(32)
        .filter_map(|p| fs::read_to_string(p).ok())
        .collect()
}

fn collect_path_completions(
    cwd: &str,
    prefix: &str,
    dirs_only: bool,
    out: &mut Vec<(i32, CompletionItem)>,
) {
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
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if dirs_only && !is_dir {
            continue;
        }
        let Some((score, idxs)) = fuzzy_match_score(&name, file_prefix) else {
            continue;
        };
        let meta = entry.metadata().ok();
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
    // 1. Skip any whitespace right before the cursor.
    while i > 0 {
        let p = prev_char_boundary(s, i);
        let ch = s[p..i].chars().next().unwrap_or(' ');
        if !ch.is_whitespace() {
            break;
        }
        i = p;
    }
    // 2. Skip any punctuation/path separator run (`/`, `-`, `.`, etc.) right at the cursor
    // so `Option+Backspace` / `Ctrl+W` at `ls /Volumes/Projects/|` deletes `Projects/` instead
    // of getting stuck at `/`!
    let mut skipped_sep = false;
    while i > 0 {
        let p = prev_char_boundary(s, i);
        let ch = s[p..i].chars().next().unwrap_or(' ');
        if !is_word_sep(ch) {
            break;
        }
        i = p;
        skipped_sep = true;
        // Stop after consuming one slash so `/Volumes/Projects/|` deletes `Projects/`, then `Volumes/`.
        if ch == '/' {
            break;
        }
    }
    // 3. Skip the word segment itself.
    let before_word = i;
    while i > 0 {
        let p = prev_char_boundary(s, i);
        let ch = s[p..i].chars().next().unwrap_or(' ');
        if ch.is_whitespace() || is_word_sep(ch) {
            break;
        }
        i = p;
    }
    // If we only saw a leading separator like `/` with whitespace before it, keep the deletion of that `/`.
    if i == before_word && !skipped_sep && i > 0 {
        i = prev_char_boundary(s, i);
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
    let mut skipped_sep = false;
    while i < len {
        let n = next_char_boundary(s, i);
        let ch = s[i..n].chars().next().unwrap_or(' ');
        if !is_word_sep(ch) {
            break;
        }
        i = n;
        skipped_sep = true;
        if ch == '/' {
            break;
        }
    }
    let before_word = i;
    while i < len {
        let n = next_char_boundary(s, i);
        let ch = s[i..n].chars().next().unwrap_or(' ');
        if ch.is_whitespace() || is_word_sep(ch) {
            break;
        }
        i = n;
    }
    if i == before_word && !skipped_sep && i < len {
        i = next_char_boundary(s, i);
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
