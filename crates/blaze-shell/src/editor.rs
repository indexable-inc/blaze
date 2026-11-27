//! Interactive multi-line shell editor: natural macOS editing (`Cmd+Z`/`Cmd+Shift+Z` undo/redo,
//! word/line navigation, selection, click-drag, kill rings), Fish/Nushell inline ghost
//! autosuggestion (`Right` / `Alt+Right`), IntelliJ-style live fuzzy autocomplete with
//! typed icons, column-aware pipeline suggestions, and `Ctrl+R` reverse history search.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use blaze_own::{Alloc, Share};
use blaze_sh::builtins::{BUILTIN_SPECS, find_spec};
use blaze_sh::lex::{HighlightSpan, highlight, is_incomplete};
use blaze_sh::{Bytes, EngineState, Value};
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
    Subcommand,
    Def,
    Keyword,
    Flag,
    Variable,
    Column,
    Dir,
    File,
    External,
    RemoteRepo,
    RemotePackage,
    Hint,
}

/// Semantic class of a command-position token for live & block syntax highlighting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandHighlightKind {
    Builtin,
    Abbrev,
    External,
    DataFile,
    Unknown,
}

/// Fast O(1) `$PATH` executable lookup cached per process for syntax highlighting.
pub fn is_external_command_in_path(name: &str) -> bool {
    use std::sync::OnceLock;
    static PATH_SET: OnceLock<HashSet<String>> = OnceLock::new();
    let set = PATH_SET.get_or_init(|| {
        let mut out = HashSet::new();
        if let Ok(path_var) = std::env::var("PATH") {
            for dir in path_var.split(':') {
                if dir.is_empty() {
                    continue;
                }
                let Ok(rd) = fs::read_dir(dir) else { continue };
                for entry in rd.flatten() {
                    if out.len() >= MAX_PATH_BINS {
                        break;
                    }
                    let fname = entry.file_name().to_string_lossy().into_owned();
                    if fname.is_empty() || fname.starts_with('.') {
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
                        out.insert(fname);
                    }
                }
            }
        }
        out
    });
    set.contains(name)
}

/// Classify a command-position token as Builtin, Abbrev, External `$PATH` binary, DataFile, or Unknown.
pub fn classify_command_token(
    token: &str,
    abbrevs: Option<&BTreeMap<String, String>>,
) -> CommandHighlightKind {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return CommandHighlightKind::Unknown;
    }
    if let Some(ext) = trimmed.strip_prefix('^') {
        return if ext.contains('/') || is_external_command_in_path(ext) {
            CommandHighlightKind::External
        } else {
            CommandHighlightKind::Unknown
        };
    }
    if abbrevs.is_some_and(|m| m.contains_key(trimmed)) {
        return CommandHighlightKind::Abbrev;
    }
    if find_spec(trimmed).is_some()
        || matches!(
            trimmed,
            "bgit"
                | "rust"
                | "agent"
                | "y"
                | "yazi"
                | "cd"
                | "pwd"
                | "alias"
                | "export"
                | "clear"
                | "exit"
                | "quit"
                | "help"
                | "history"
                | "fork"
                | "z"
                | "zi"
                | "source"
                | "use"
                | "hide"
                | "env"
        )
    {
        return CommandHighlightKind::Builtin;
    }
    if is_bare_data_file(trimmed) || split_data_file_dot_prefix(trimmed).is_some() {
        return CommandHighlightKind::DataFile;
    }
    if trimmed.contains('/') || is_external_command_in_path(trimmed) {
        return CommandHighlightKind::External;
    }
    CommandHighlightKind::Unknown
}

/// Cached structured data files in `cwd` (re-read only when directory mtime changes).
#[derive(Clone)]
struct CwdDataFileEntry {
    name: String,
    size_bytes: u64,
    ext: String,
    sniff_label: &'static str,
}

fn cached_cwd_data_files(cwd: &str) -> Vec<CwdDataFileEntry> {
    use std::cell::RefCell;
    use std::time::SystemTime;
    struct Cache {
        cwd: String,
        mtime: Option<SystemTime>,
        entries: Vec<CwdDataFileEntry>,
    }
    thread_local! {
        static CACHE: RefCell<Option<Cache>> = const { RefCell::new(None) };
    }
    let mtime = fs::metadata(cwd).ok().and_then(|m| m.modified().ok());
    CACHE.with(|cell| {
        if let Some(hit) = cell.borrow().as_ref()
            && hit.cwd == cwd
            && hit.mtime == mtime
        {
            return hit.entries.clone();
        }
        let mut entries = Vec::new();
        if let Ok(rd) = fs::read_dir(cwd) {
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.is_empty() || name.starts_with('.') || !is_bare_data_file(&name) {
                    continue;
                }
                let Ok(meta) = entry.metadata() else { continue };
                if !meta.is_file() {
                    continue;
                }
                let ext = name
                    .rsplit_once('.')
                    .map(|(_, e)| e.to_ascii_lowercase())
                    .unwrap_or_default();
                let sniff = blaze_sniff::sniff_file(&name, &[]);
                entries.push(CwdDataFileEntry {
                    name,
                    size_bytes: meta.len(),
                    ext,
                    sniff_label: sniff.label,
                });
            }
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        *cell.borrow_mut() = Some(Cache {
            cwd: cwd.to_owned(),
            mtime,
            entries: entries.clone(),
        });
        entries
    })
}

/// True when `prefix` matches the start of any known builtin, abbreviation, `$PATH` binary,
/// or structured data file in `cwd` (case-insensitive so typing `Car` stays neutral instead
/// of flashing red mid-word).
pub fn is_known_command_prefix(
    prefix: &str,
    cwd: Option<&str>,
    abbrevs: Option<&BTreeMap<String, String>>,
) -> bool {
    if prefix.is_empty() || prefix.starts_with('/') || prefix.starts_with('.') || prefix.starts_with('~') {
        return true;
    }
    let clean = prefix.strip_prefix('^').unwrap_or(prefix);
    let clean_lower = clean.to_ascii_lowercase();
    if abbrevs.is_some_and(|m| m.keys().any(|k| k.to_ascii_lowercase().starts_with(&clean_lower))) {
        return true;
    }
    if BUILTIN_SPECS.iter().any(|s| s.name.starts_with(&clean_lower)) || "bgit".starts_with(&clean_lower) {
        return true;
    }
    if KEYWORDS.iter().any(|(k, _)| k.starts_with(&clean_lower)) {
        return true;
    }
    if let Some(dir) = cwd
        && cached_cwd_data_files(dir)
            .iter()
            .any(|f| f.name.to_ascii_lowercase().starts_with(&clean_lower))
    {
        return true;
    }
    if let Ok(path_var) = std::env::var("PATH") {
        use std::sync::OnceLock;
        static SORTED_LOWER_BINS: OnceLock<Vec<String>> = OnceLock::new();
        let bins = SORTED_LOWER_BINS.get_or_init(|| {
            let mut seen = HashSet::new();
            let mut out = Vec::new();
            for dir in path_var.split(':') {
                if dir.is_empty() {
                    continue;
                }
                let Ok(rd) = fs::read_dir(dir) else { continue };
                for entry in rd.flatten() {
                    if out.len() >= MAX_PATH_BINS {
                        break;
                    }
                    let fname = entry.file_name().to_string_lossy().into_owned();
                    if fname.is_empty() || fname.starts_with('.') {
                        continue;
                    }
                    let lower = fname.to_ascii_lowercase();
                    if seen.insert(lower.clone()) {
                        out.push(lower);
                    }
                }
            }
            out.sort_unstable();
            out
        });
        let idx = bins.partition_point(|b| b.as_str() < clean_lower.as_str());
        if let Some(cand) = bins.get(idx)
            && cand.starts_with(&clean_lower)
        {
            return true;
        }
    }
    false
}

/// Filesystem metadata attached to path/directory completion candidates for `ls`-matching
/// recency heatmap coloring and proportional tier-colored size bars.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionFsMeta {
    pub path: String,
    pub mtime_ns: Option<i64>,
    pub size_bytes: Option<u64>,
    /// True when this candidate is a direct filesystem child of the searched directory
    /// (as opposed to a distant zoxide history entry or deep git subpath).
    pub is_direct_child: bool,
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
    /// Optional local cached Retina circular avatar PNG/JPEG path (e.g. GitHub org/owner avatar).
    pub avatar_path: Option<String>,
    /// Optional filesystem path, mtime, and byte size for recency heatmap + size bar rendering.
    pub fs_meta: Option<CompletionFsMeta>,
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
    ToggleSidebar,
    NewTiledPane,
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
    PaletteCommand { title: "New Tiled Pane (Auto Split)", category: "Tiling", shortcut: "⌘N", action: PaletteAction::NewTiledPane },
    PaletteCommand { title: "Split Pane Vertically (Right)", category: "Tiling", shortcut: "⌘D", action: PaletteAction::SplitVertical },
    PaletteCommand { title: "Split Pane Horizontally (Down)", category: "Tiling", shortcut: "⌘⇧D", action: PaletteAction::SplitHorizontal },
    PaletteCommand { title: "Toggle Zoom Focused Pane", category: "Tiling", shortcut: "⌘⇧↵", action: PaletteAction::ToggleZoom },
    PaletteCommand { title: "Equalize Split Ratios", category: "Tiling", shortcut: "⌘⌃=", action: PaletteAction::EqualizeSplits },
    PaletteCommand { title: "Close Focused Pane / Tab", category: "Tiling", shortcut: "⌘W", action: PaletteAction::ClosePane },
    PaletteCommand { title: "New Workspace Tab", category: "Workspace", shortcut: "⌘T", action: PaletteAction::NewTab },
    PaletteCommand { title: "Toggle Workspace Sidebar", category: "Workspace", shortcut: "⌘S", action: PaletteAction::ToggleSidebar },
    PaletteCommand { title: "New OS Window (Separate)", category: "Window", shortcut: "⌘⇧N", action: PaletteAction::NewWindow },
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
    pub completion_replace_start: usize,
    /// When the user explicitly presses `Esc` to dismiss auto-opened completions on a token,
    /// remember that token's start offset so we don't immediately re-open until they move on.
    completions_suppressed_token_start: Option<usize>,
    /// Fish/Nushell-style dimmed inline ghost suffix shown directly after the cursor.
    pub ghost: Option<String>,
    /// Sans-IO Vim modal editing engine (`Normal`/`Insert`/`Visual`/`VisualLine`, undo/redo,
    /// registers, `.` repeat, and pluggable `ShellObjects` including `i|`/`i/`/`i,`).
    vim: VimMachine<'static>,
    last_edit_op: EditOp,
    /// Lazily cached external executable names from `$PATH`.
    path_bins: Option<Vec<(String, String)>>,
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
    ("cl", "claude --dangerously-skip-permissions"),
    ("cls", "clear"),
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
    ("bb", "blaze build"),
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
    ("gw", "git switch"),
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
    "sort",
    "sort-by",
    "top",
    "freq",
    "find",
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
            completions_suppressed_token_start: None,
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
        // User `[abbreviations]` in `~/.config/blaze-shell/config.toml` take top priority.
        let cfg = crate::config::load_cached(home);
        for entry in cfg.abbreviations.iter() {
            self.abbreviations
                .insert(entry.name.as_str().to_owned(), entry.desc.as_str().to_owned());
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
            self.sync_ghost_to_selected_completion();
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
            self.sync_ghost_to_selected_completion();
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

    /// `Cmd+Backspace`: delete active selection, kill from start of current line to cursor,
    /// or join with the line above when at column 0 of a continuation line.
    pub fn kill_to_line_start(&mut self) {
        if self.selection_range().is_some() {
            self.push_undo(EditOp::Other);
            self.delete_selection_raw();
            if self.vim.visual_selection(&self.text).is_some() {
                self.vim.set_mode(&self.text, VimMode::Insert);
            }
            self.vim.set_cursor(&self.text, self.cursor);
            self.history_pos = None;
            self.ghost = None;
            self.dismiss_completions();
            return;
        }
        self.clamp_cursor();
        if self.cursor == 0 {
            self.selection_anchor = None;
            return;
        }
        let line_start = self.text[..self.cursor].rfind('\n').map_or(0, |p| p + 1);
        self.push_undo(EditOp::Other);
        if self.cursor > line_start {
            self.text.drain(line_start..self.cursor);
            self.cursor = line_start;
        } else {
            // `cursor == line_start && cursor > 0`: remove the single preceding `'\n'`
            // so `Cmd+Backspace` on an empty continuation line joins with the line above cleanly.
            self.text.drain(self.cursor - 1..self.cursor);
            self.cursor -= 1;
        }
        self.selection_anchor = None;
        self.vim.set_cursor(&self.text, self.cursor);
        self.history_pos = None;
        self.ghost = None;
        self.dismiss_completions();
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

    /// Explicitly dismiss the completion popup (e.g. on `Esc`) and suppress zero-Tab auto-opening
    /// until the user moves to a different token or presses `Tab`.
    pub fn suppress_completions(&mut self) {
        self.clamp_cursor();
        let tok_start = token_start_offset(&self.text[..self.cursor]);
        self.completions_suppressed_token_start = Some(tok_start);
        self.dismiss_completions();
    }

    /// Keep the inline Fish ghost text 100% consistent with the currently highlighted row in the
    /// completion popup whenever the cursor is at the end of a single-line buffer.
    fn sync_ghost_to_selected_completion(&mut self) {
        let at_eol = self.cursor == self.text.len() && !self.text.contains('\n');
        if !at_eol || self.completions.is_empty() {
            return;
        }
        let Some(sel) = self.completions.get(self.completion_selected) else {
            return;
        };
        if sel.kind == CompletionKind::Hint {
            return;
        }
        let start = clamp_char_boundary(&self.text, self.completion_replace_start).min(self.cursor);
        let prefix = &self.text[start..self.cursor];
        // Require EXACT case match so typing `Car` never synthesizes bogus hybrid ghost text
        // like `Car` + `go-rm` = `Cargo-rm` when a lowercase binary is highlighted.
        if sel.insert.len() > prefix.len() && sel.insert.starts_with(prefix) {
            let suffix = &sel.insert[prefix.len()..];
            // Preserve a richer full-command ghost (e.g. `git log --oneline -n 20` or
            // `ls crates/blaze-sh/src | sort-by size -r`) when it already starts with the
            // selected token followed by whitespace or `/`.
            let keep_richer_ghost = self.ghost.as_deref().is_some_and(|g| {
                g.starts_with(suffix)
                    && (suffix.ends_with('/')
                        || g.as_bytes()
                            .get(suffix.len())
                            .is_none_or(|b| b.is_ascii_whitespace() || *b == b'/'))
            });
            if !keep_richer_ghost {
                self.ghost = Some(suffix.to_owned());
            }
        }
    }

    /// Unified auto-open rule: autocomplete opens BY DEFAULT as you type in Insert mode on a
    /// single-line prompt (zero Tab required!), while staying out of the way inside comments,
    /// quoted string literals, pure numbers, or finished leaf subcommands where the 1-line
    /// signature hint should show instead.
    fn should_auto_open_completions(
        &self,
        before: &str,
        token_start: usize,
        prefix: &str,
    ) -> bool {
        if self.vim_mode() != VimMode::Insert || self.text.contains('\n') {
            return false;
        }
        // Stay out of the way inside `# comments` or unclosed string literals (`"..."`, `'...'`).
        if is_inside_comment_or_quotes(before) {
            return false;
        }

        let prev_non_ws = before[..token_start].trim_end();
        let at_expr_start = prev_non_ws.is_empty()
            || prev_non_ws.ends_with('|')
            || prev_non_ws.ends_with(';')
            || prev_non_ws.ends_with('(');
        let is_dot_subject_path = at_expr_start
            && prefix.starts_with('.')
            && !prefix.starts_with("./")
            && !prefix.starts_with("..");
        let is_data_file_dot_path = at_expr_start && split_data_file_dot_prefix(prefix).is_some();
        let is_var_dot_path = prefix.starts_with('$') && prefix.contains('.');
        if is_dot_subject_path
            || is_data_file_dot_path
            || is_var_dot_path
            || prefix.starts_with('-')
            || prev_non_ws.ends_with('|')
            || (before.ends_with(' ') && expects_column_arg(before))
            || (before.ends_with(' ')
                && blaze_cli_spec::remote::detect_remote_context(before, Alloc::global()).is_some())
        {
            return true;
        }

        // Pure numeric literals (`42`, `3.14`, `0xff`) don't need popup noise.
        if !prefix.is_empty() && is_pure_number_token(prefix) {
            return false;
        }

        let stage_start = before.rfind(['|', ';', '\n']).map_or(0, |p| p + 1);
        let stage_raw = &before[stage_start..];
        let stage_trimmed = stage_raw.trim_start();
        if stage_trimmed.is_empty() {
            return false;
        }
        let trailing_space = stage_raw.ends_with(char::is_whitespace);
        let words: Vec<&str> = stage_trimmed.split_whitespace().collect();
        if words.is_empty() {
            return false;
        }

        // First token (command / bare data file / variable): auto-open by default as soon as >= 1 char is typed!
        if words.len() == 1 && !trailing_space {
            return !prefix.is_empty();
        }

        let cmd_base = words[0].trim_start_matches('^').rsplit('/').next().unwrap_or(words[0]);
        let done_words = if trailing_space {
            &words[1..]
        } else {
            &words[1..words.len() - 1]
        };
        let has_done_sub = done_words.iter().any(|w| !w.starts_with('-'));

        // Keep exact unique leaf subcommands (`git add`, `bgit wt add `) quiet so the 1-line
        // `LiveHint::Signature` bar shows cleanly without a redundant 1-row popup.
        if cmd_base == "bgit" {
            let active = resolve_bgit_spec(done_words);
            if active.subcommands.is_empty() {
                return !prefix.is_empty();
            }
            if !prefix.is_empty()
                && let Some(exact) = active
                    .subcommands
                    .iter()
                    .find(|s| !s.hidden && (s.name == prefix || s.aliases.contains(&prefix)))
            {
                let matching_count = active
                    .subcommands
                    .iter()
                    .filter(|s| !s.hidden && fuzzy_match_score(s.name, prefix).is_some())
                    .count();
                if matching_count <= 1 && exact.spec.subcommands.is_empty() {
                    return false;
                }
            }
            return true;
        }

        if cmd_base == "git" {
            if has_done_sub {
                return !prefix.is_empty();
            }
            if !prefix.is_empty()
                && GIT_SUBCOMMAND_SIGNATURES.iter().any(|m| m.name == prefix)
                && GIT_SUBCOMMANDS
                    .iter()
                    .filter(|&&(name, _)| name.starts_with(prefix))
                    .count()
                    <= 1
            {
                return false;
            }
            return true;
        }

        if cmd_base == "blaze" {
            if has_done_sub {
                return !prefix.is_empty();
            }
            let app = &blaze_cli_spec::blaze_app::BLAZE_APP;
            if !prefix.is_empty()
                && blaze_cli_spec::app::find_command(app, prefix).is_some()
                && app
                    .commands
                    .iter()
                    .filter(|c| !c.hidden && c.name.starts_with(prefix))
                    .count()
                    <= 1
            {
                return false;
            }
            return true;
        }

        if !has_done_sub
            && let Some(cli) = blaze_cli_spec::manual::manual_spec_in(cmd_base, None, Alloc::global())
            && !cli.subcommands.is_empty()
        {
            if !prefix.is_empty()
                && cli.subcommands.iter().any(|s| s.name.as_str() == prefix)
                && cli
                    .subcommands
                    .iter()
                    .filter(|s| s.name.as_str().starts_with(prefix))
                    .count()
                    <= 1
            {
                return false;
            }
            return true;
        }

        // Auto-open immediately after space for commands whose primary argument is a live set
        // (`cd `, `z `, `open `, `ssh `, `sftp `, `mosh `), and ALWAYS whenever typing any argument prefix!
        if !prefix.is_empty() {
            return true;
        }
        matches!(cmd_base, "cd" | "z" | "open" | "ssh" | "sftp" | "mosh")
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
        let home = std::env::var("HOME").unwrap_or_default();
        let mut seen: HashSet<String> = HashSet::new();
        let mut bins: Vec<(String, String)> = Vec::new();
        if let Ok(path_var) = std::env::var("PATH") {
            for dir in path_var.split(':') {
                if dir.is_empty() {
                    continue;
                }
                let Ok(rd) = fs::read_dir(dir) else { continue };
                let short_dir = crate::prompt::abbreviate_home(dir, &home);
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
                        bins.push((name, short_dir.clone()));
                    }
                }
            }
        }
        bins.sort_unstable_by(|a, b| a.0.cmp(&b.0));
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
            self.completions_suppressed_token_start = None;
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

        let before = &self.text[..self.cursor];
        let token_start = token_start_offset(before);
        let prefix = before[token_start..].to_owned();
        let prev_non_ws = before[..token_start].trim_end();

        // Moving to a new token clears any previous `Esc` suppression.
        if self.completions_suppressed_token_start.is_some_and(|s| s != token_start) {
            self.completions_suppressed_token_start = None;
        }
        let is_suppressed = self.completions_suppressed_token_start == Some(token_start);

        let auto_open = !is_suppressed && self.should_auto_open_completions(before, token_start, &prefix);
        if !open_popup && !auto_open {
            if !self.completions.is_empty() {
                self.dismiss_completions();
            }
            return;
        }

        // Trigger live IntelliJ popup when typing a token (>= 1 char), `$`, `-`, `/`, after `| `,
        // after a remote-search command, or when auto-opened for structured CLI subcommands/flags.
        let should_show = auto_open
            || !prefix.is_empty()
            || prev_non_ws.ends_with('|')
            || (before.ends_with(' ') && expects_column_arg(before))
            || (before.ends_with(' ')
                && blaze_cli_spec::remote::detect_remote_context(before, blaze_own::Alloc::global()).is_some());

        if !should_show {
            self.dismiss_completions();
            return;
        }

        let prev_selected_label = if open_popup && self.completion_selected > 0 {
            self.completions
                .get(self.completion_selected)
                .map(|c| c.label.clone())
        } else {
            None
        };
        let items = self.compute_completions(state, recent_cols, token_start, &prefix);
        // Hide popup if the only candidate is already typed verbatim.
        if items.len() == 1 && items[0].kind != CompletionKind::Hint && items[0].insert == prefix {
            self.dismiss_completions();
            return;
        }

        if items.is_empty() {
            self.dismiss_completions();
        } else {
            // Always highlight the #1 most likely completion (index 0) unless the user
            // explicitly arrowed to a specific candidate while keeping the popup open.
            let new_sel = prev_selected_label
                .as_deref()
                .and_then(|lbl| items.iter().position(|it| it.label == lbl))
                .unwrap_or(0);
            self.completion_replace_start = token_start;
            self.completions = items;
            self.completion_selected = new_sel;
            self.sync_completion_scroll();
            self.sync_ghost_to_selected_completion();
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
        self.completions_suppressed_token_start = None;
        if !self.completions.is_empty() {
            if self.completions.len() == 1 {
                self.apply_selected_completion();
            } else {
                self.completion_selected = (self.completion_selected + 1) % self.completions.len();
                self.sync_completion_scroll();
                self.sync_ghost_to_selected_completion();
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

        if self.completions.len() == 1 && self.completions[0].kind != CompletionKind::Hint {
            self.apply_selected_completion();
            return;
        }

        // If the typed prefix uniquely matches ONE direct child of the current/target directory
        // (e.g. `cd rg<Tab>` inside `big-repos/` with `ripgrep/` even when distant zoxide also
        // matches `/Volumes/.../rgb`), expand that local child immediately!
        let direct_children: Vec<usize> = self
            .completions
            .iter()
            .enumerate()
            .filter(|(_, it)| it.fs_meta.as_ref().is_some_and(|f| f.is_direct_child))
            .map(|(i, _)| i)
            .collect();
        if !prefix.is_empty() && direct_children.len() == 1 {
            self.apply_completion(direct_children[0]);
            return;
        }

        // First-Tab longest-common-prefix completion (classic Fish/Zsh/Nushell ergonomics):
        // e.g. `cd /Vo<Tab>` with `Volumes/` + 20 zoxide `/Volumes/...` descendants immediately
        // completes `/Volumes/` so the user can keep typing `Proj<Tab>` -> `/Volumes/Projects/`.
        // When direct local children exist, compute LCP across them first so distant zoxide
        // entries can never block local common-prefix expansion.
        let lcp = if !direct_children.is_empty() && !prefix.is_empty() {
            let local_items: Vec<CompletionItem> = direct_children
                .iter()
                .map(|&i| self.completions[i].clone())
                .collect();
            let local_lcp = longest_common_prefix(&local_items);
            if local_lcp.len() > prefix.len() {
                local_lcp
            } else {
                longest_common_prefix(&self.completions)
            }
        } else {
            longest_common_prefix(&self.completions)
        };
        if lcp.len() > prefix.len()
            && lcp
                .get(..prefix.len())
                .is_some_and(|p| p.eq_ignore_ascii_case(&prefix))
        {
            let is_exact_candidate = self
                .completions
                .iter()
                .any(|it| it.kind != CompletionKind::Hint && it.insert == lcp);
            self.push_undo(EditOp::Other);
            self.clamp_cursor();
            let start = clamp_char_boundary(&self.text, self.completion_replace_start).min(self.cursor);
            self.text.replace_range(start..self.cursor, &lcp);
            self.cursor = start + lcp.len();
            self.selection_anchor = None;
            self.ghost = None;
            if is_exact_candidate {
                self.dismiss_completions();
            } else {
                let new_before = &self.text[..self.cursor];
                let new_tok_start = token_start_offset(new_before);
                let new_prefix = new_before[new_tok_start..].to_owned();
                let narrowed = self.compute_completions(state, recent_cols, new_tok_start, &new_prefix);
                if narrowed.is_empty() {
                    self.dismiss_completions();
                } else {
                    self.completion_replace_start = new_tok_start;
                    self.completions = narrowed;
                    self.completion_selected = 0;
                    self.completion_scroll = 0;
                    self.sync_ghost_to_selected_completion();
                }
            }
        } else {
            self.sync_ghost_to_selected_completion();
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
            "blaze build",
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

        let at_expr_start = at_cmd_pos || prev_non_ws.ends_with('(');
        let mut scored: Vec<(i32, CompletionItem)> = Vec::new();

        if at_expr_start
            && prefix.starts_with('.')
            && !prefix.starts_with("./")
            && !prefix.starts_with("..")
        {
            if prev_non_ws.ends_with('|') {
                let before_pipe = &prev_non_ws[..prev_non_ws.len() - 1];
                if before_pipe.trim().is_empty() {
                    if let Some(slot) = state.get_var("_") {
                        collect_value_path_completions(&slot.value, "", &prefix[1..], &mut scored);
                    }
                } else if let Some(piped_val) = resolve_piped_value_for_dot(before_pipe, state) {
                    collect_value_path_completions(&piped_val, "", &prefix[1..], &mut scored);
                } else {
                    collect_piped_command_schema_dot_completions(before_pipe, &prefix[1..], &mut scored);
                }
            } else if let Some(slot) = state.get_var("_") {
                collect_value_path_completions(&slot.value, "", &prefix[1..], &mut scored);
            }
        } else if at_expr_start
            && let Some((file_part, rest_after_dot)) = split_data_file_dot_prefix(prefix)
        {
            let abs = resolve_preview_path(state.cwd.as_ref(), file_part, state.get_env("HOME"));
            if let Some(parsed) = load_cached_open_preview(&abs) {
                collect_value_path_completions(&parsed, file_part, rest_after_dot, &mut scored);
            }
        } else if let Some(var_prefix) = prefix.strip_prefix('$') {
            if let Some((var_name, rest_after_dot)) = var_prefix.split_once('.') {
                let base_prefix = format!("${var_name}");
                if var_name == "env" {
                    let env_val = Value::Record(state.env_record());
                    collect_value_path_completions(&env_val, &base_prefix, rest_after_dot, &mut scored);
                } else if let Some(slot) = state.get_var(var_name) {
                    collect_value_path_completions(&slot.value, &base_prefix, rest_after_dot, &mut scored);
                }
            } else {
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
                                avatar_path: None,
                                fs_meta: None,
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
                                avatar_path: None,
                                fs_meta: None,
                                match_indices: idxs,
                            },
                        ));
                    }
                });
            }
        } else if prefix.starts_with('-') {
            let stage_start = before.rfind(['|', ';', '\n']).map_or(0, |p| p + 1);
            let stage_prefix = before[stage_start..token_start].trim();
            let stage_words: Vec<&str> = stage_prefix.split_whitespace().collect();
            let cmd0 = stage_words.first().copied().unwrap_or("");
            let cmd_base = cmd0.trim_start_matches('^').rsplit('/').next().unwrap_or(cmd0);
            let subcmd = stage_words
                .get(1)
                .copied()
                .filter(|w| !w.starts_with('-'));
            let mut seen_flags: HashSet<String> = HashSet::new();

            // 1. Native `blaze-sh` builtin flags, curated `git`/`blaze` flags, and `bgit` `bstd::cli` flags/opts.
            if cmd_base == "git"
                && let Some(sub) = subcmd
                && let Some(meta) = GIT_SUBCOMMAND_SIGNATURES.iter().find(|m| m.name == sub)
            {
                for (idx, &(tok, _short, desc)) in meta.flags.iter().enumerate() {
                    if let Some((score, idxs)) = fuzzy_match_score(tok, prefix)
                        && seen_flags.insert(tok.to_owned())
                    {
                        let rank = (meta.flags.len().saturating_sub(idx)).min(16) as i32;
                        scored.push((
                            score + 65 + rank,
                            CompletionItem {
                                label: tok.to_owned(),
                                insert: tok.to_owned(),
                                detail: String::new(),
                                doc: desc.to_owned(),
                                kind: CompletionKind::Flag,
                                icon: None,
                                avatar_path: None,
                                fs_meta: None,
                                match_indices: idxs,
                            },
                        ));
                    }
                }
            }
            if cmd_base == "blaze" {
                let app = &blaze_cli_spec::blaze_app::BLAZE_APP;
                if let Some(sub) = subcmd
                    && let Some(cmd) = blaze_cli_spec::app::find_command(app, sub)
                {
                    for fl in cmd.flags.iter().chain(app.common_flags.iter()) {
                        if fl.hidden {
                            continue;
                        }
                        let Some(tok) = fl
                            .long
                            .map(|l| format!("--{l}"))
                            .or_else(|| fl.short.map(|c| format!("-{c}")))
                        else {
                            continue;
                        };
                        if let Some((score, idxs)) = fuzzy_match_score(&tok, prefix)
                            && seen_flags.insert(tok.clone())
                        {
                            let detail = fl.short.map(|c| format!("-{c}")).unwrap_or_default();
                            scored.push((
                                score + 65,
                                CompletionItem {
                                    label: tok.clone(),
                                    insert: tok,
                                    detail,
                                    doc: fl.about.to_owned(),
                                    kind: CompletionKind::Flag,
                                    icon: None,
                                    avatar_path: None,
                                    fs_meta: None,
                                    match_indices: idxs,
                                },
                            ));
                        }
                    }
                }
            }
            if cmd_base == "bgit" {
                let spec = resolve_bgit_spec(&stage_words[1..]);
                for f in spec.flags {
                    if f.hidden {
                        continue;
                    }
                    let long_tok = format!("--{}", f.long);
                    if let Some((score, idxs)) = fuzzy_match_score(&long_tok, prefix)
                        && seen_flags.insert(long_tok.clone())
                    {
                        scored.push((
                            score + 60,
                            CompletionItem {
                                label: long_tok.clone(),
                                insert: long_tok,
                                detail: "bgit".to_owned(),
                                doc: f.help.to_owned(),
                                kind: CompletionKind::Flag,
                                icon: None,
                                avatar_path: None,
                                fs_meta: None,
                                match_indices: idxs,
                            },
                        ));
                    }
                }
                for o in spec.opts {
                    if o.hidden {
                        continue;
                    }
                    let long_tok = format!("--{}", o.long);
                    if let Some((score, idxs)) = fuzzy_match_score(&long_tok, prefix)
                        && seen_flags.insert(long_tok.clone())
                    {
                        scored.push((
                            score + 60,
                            CompletionItem {
                                label: format!("{long_tok} <{}>", o.value_name),
                                insert: long_tok,
                                detail: "bgit".to_owned(),
                                doc: o.help.to_owned(),
                                kind: CompletionKind::Flag,
                                icon: None,
                                avatar_path: None,
                                fs_meta: None,
                                match_indices: idxs,
                            },
                        ));
                    }
                }
            }
            // Descending `-col` shortcuts for `sort` / `sort-by` (e.g. `Cargo.lock.package | sort -name`).
            if matches!(cmd_base, "sort" | "sort-by") && !prefix.starts_with("--") {
                let col_query = prefix.trim_start_matches('-');
                for (col_name, col_ty) in infer_pipeline_columns(before, state, recent_cols) {
                    if let Some((score, mut idxs)) = fuzzy_match_score(&col_name, col_query) {
                        for i in &mut idxs {
                            *i += 1; // offset past `-`
                        }
                        let neg_tok = format!("-{col_name}");
                        if seen_flags.insert(neg_tok.clone()) {
                            scored.push((
                                score + 75,
                                CompletionItem {
                                    label: neg_tok.clone(),
                                    insert: neg_tok,
                                    detail: format!("desc · {col_ty}"),
                                    doc: format!("sort by `{col_name}` descending"),
                                    kind: CompletionKind::Column,
                                    icon: None,
                                    avatar_path: None,
                                    fs_meta: None,
                                    match_indices: idxs,
                                },
                            ));
                        }
                    }
                }
            }
            if let Some(spec) = find_spec(cmd_base) {
                for &flag in spec.flags {
                    if let Some((score, idxs)) = fuzzy_match_score(flag, prefix)
                        && seen_flags.insert(flag.to_owned())
                    {
                        scored.push((
                            score + 40,
                            CompletionItem {
                                label: flag.to_owned(),
                                insert: flag.to_owned(),
                                detail: "flag".to_owned(),
                                doc: format!("{} {flag} — {}", spec.name, spec.summary),
                                kind: CompletionKind::Flag,
                                icon: None,
                                avatar_path: None,
                                fs_meta: None,
                                match_indices: idxs,
                            },
                        ));
                    }
                }
            }

            // 2. User manual overrides from `~/.config/blaze-shell/config.toml` (`[completions.<cmd>.flags]`).
            let home = state.get_env("HOME").unwrap_or("");
            let user_cfg = crate::config::load_cached(home);
            if let Some(ov) = user_cfg.completion_override(cmd_base) {
                for entry in ov.flags.iter() {
                    let flag = entry.name.as_str();
                    if let Some((score, idxs)) = fuzzy_match_score(flag, prefix)
                        && seen_flags.insert(flag.to_owned())
                    {
                        scored.push((
                            score + 60,
                            CompletionItem {
                                label: flag.to_owned(),
                                insert: flag.to_owned(),
                                detail: "custom".to_owned(),
                                doc: if entry.desc.as_str().is_empty() {
                                    format!("{cmd_base} {flag}")
                                } else {
                                    entry.desc.as_str().to_owned()
                                },
                                kind: CompletionKind::Flag,
                                icon: None,
                                avatar_path: None,
                                fs_meta: None,
                                match_indices: idxs,
                            },
                        ));
                    }
                }
            }

            // 3. Universal non-blocking CLI spec parser (`--help`, fish completions, `man`, curated manual specs).
            let path_env = state
                .get_env("PATH")
                .map(str::to_owned)
                .or_else(|| std::env::var("PATH").ok())
                .unwrap_or_default();
            let home_opt = (!home.is_empty()).then_some(home);
            let mut spec_sources = Vec::new();
            if let Some(sub) = subcmd
                && let Some(sub_spec) =
                    crate::cli_spec_driver::lookup_nonblocking(cmd_base, Some(sub), &path_env, home_opt)
            {
                spec_sources.push((55i32, sub_spec));
            }
            if let Some(top_spec) =
                crate::cli_spec_driver::lookup_nonblocking(cmd_base, None, &path_env, home_opt)
            {
                spec_sources.push((35i32, top_spec));
            }

            for (boost, spec) in spec_sources {
                for f in spec.flags.iter() {
                    let hint = f.value_hint.as_ref().map(|h| h.as_str());
                    let desc = f.description.as_str();
                    let short_s = f.short.as_ref().map(|s| s.as_str());
                    let long_s = f.long.as_ref().map(|s| s.as_str());

                    if let Some(long) = long_s
                        && let Some((score, idxs)) = fuzzy_match_score(long, prefix)
                        && seen_flags.insert(long.to_owned())
                    {
                        let detail = match (short_s, hint) {
                            (Some(s), Some(h)) => format!("{s}  {h}"),
                            (Some(s), None) => s.to_owned(),
                            (None, Some(h)) => h.to_owned(),
                            (None, None) => "flag".to_owned(),
                        };
                        let doc = if !desc.is_empty() {
                            desc.to_owned()
                        } else {
                            format!("{cmd_base} {long}")
                        };
                        scored.push((
                            score + boost,
                            CompletionItem {
                                label: long.to_owned(),
                                insert: long.to_owned(),
                                detail,
                                doc,
                                kind: CompletionKind::Flag,
                                icon: None,
                                avatar_path: None,
                                fs_meta: None,
                                match_indices: idxs,
                            },
                        ));
                    }

                    // Also match short flags when user typed `-` (not `--`).
                    if !prefix.starts_with("--")
                        && let Some(short) = short_s
                        && let Some((score, idxs)) = fuzzy_match_score(short, prefix)
                        && seen_flags.insert(short.to_owned())
                    {
                        let detail = match (long_s, hint) {
                            (Some(l), _) => l.to_owned(),
                            (None, Some(h)) => h.to_owned(),
                            (None, None) => "flag".to_owned(),
                        };
                        let doc = if !desc.is_empty() {
                            desc.to_owned()
                        } else {
                            format!("{cmd_base} {short}")
                        };
                        scored.push((
                            score + boost - 5,
                            CompletionItem {
                                label: short.to_owned(),
                                insert: short.to_owned(),
                                detail,
                                doc,
                                kind: CompletionKind::Flag,
                                icon: None,
                                avatar_path: None,
                                fs_meta: None,
                                match_indices: idxs,
                            },
                        ));
                    }
                }
            }
        } else if at_cmd_pos {
            let after_pipe = prev_non_ws.ends_with('|');
            let is_explicit_path = prefix.starts_with("./")
                || prefix.starts_with("../")
                || prefix.starts_with('/')
                || prefix.starts_with("~/");
            if !after_pipe && is_explicit_path {
                collect_path_completions(state.cwd.as_ref(), prefix, false, &mut scored);
            } else {
                // First-class bare structured data files in CWD (`Cargo.lock`, `Cargo.toml`,
                // `blaze.lock`, `data.json`, `items.csv`) — exact-case matches like `Car` ->
                // `Cargo.lock` / `Cargo.toml` jump straight to the top!
                if !after_pipe && !prefix.is_empty() {
                    for f in cached_cwd_data_files(state.cwd.as_ref()) {
                        if let Some((score, idxs)) = fuzzy_match_score(&f.name, prefix) {
                            let sz = crate::view::format_value_cell(&blaze_sh::Value::Size(Bytes(
                                f.size_bytes.min(i64::MAX as u64) as i64,
                            )));
                            let doc = if f.sniff_label.is_empty() || f.sniff_label == "Plain text" {
                                format!("data · {sz}")
                            } else {
                                format!("{} · {sz}", f.sniff_label)
                            };
                            scored.push((
                                score + 65,
                                CompletionItem {
                                    label: f.name.clone(),
                                    insert: f.name.clone(),
                                    detail: f.ext,
                                    doc,
                                    kind: CompletionKind::File,
                                    icon: Some(icon_name_for_file(&f.name, Some("file"))),
                                    avatar_path: None,
                                    fs_meta: None,
                                    match_indices: idxs,
                                },
                            ));
                        }
                    }
                }
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
                                    avatar_path: None,
                                    fs_meta: None,
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
                                doc: spec.summary.to_owned(),
                                kind: CompletionKind::Builtin,
                                icon: None,
                                avatar_path: None,
                                fs_meta: None,
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
                                avatar_path: None,
                                fs_meta: None,
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
                                avatar_path: None,
                                fs_meta: None,
                                match_indices: idxs,
                            },
                        ));
                    }
                }
                // External PATH commands: score ALL matches first and keep the top 24 by score
                // (never stop at the first 24 alphabetical matches!) so `cargo` always outranks
                // `cargo-add` / `cargo-buckal` / `cargo-rm`.
                if !prefix.is_empty() {
                    self.ensure_path_bins();
                    if let Some(bins) = &self.path_bins {
                        let mut ext_matches: Vec<(i32, usize, Vec<usize>)> = Vec::new();
                        for (idx, (bin, _)) in bins.iter().enumerate() {
                            if let Some((score, idxs)) = fuzzy_match_score(bin, prefix) {
                                ext_matches.push((score, idx, idxs));
                            }
                        }
                        ext_matches.sort_by(|a, b| {
                            b.0.cmp(&a.0).then_with(|| bins[a.1].0.cmp(&bins[b.1].0))
                        });
                        for (score, idx, idxs) in ext_matches.into_iter().take(24) {
                            let (bin, dir) = &bins[idx];
                            scored.push((
                                score,
                                CompletionItem {
                                    label: bin.clone(),
                                    insert: bin.clone(),
                                    detail: "bin".to_owned(),
                                    doc: dir.clone(),
                                    kind: CompletionKind::External,
                                    icon: None,
                                    avatar_path: None,
                                    fs_meta: None,
                                    match_indices: idxs,
                                },
                            ));
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
                                avatar_path: None,
                                fs_meta: None,
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
                    for (idx, &(sub, desc)) in GIT_SUBCOMMANDS.iter().enumerate() {
                        if let Some((score, idxs)) = fuzzy_match_score(sub, prefix) {
                            // Preserve curated frequency ordering (`status`, `diff`, `log`, `switch`, …)
                            // on prefix ties so `status` always ranks above `stash` on `git s`/`git st`.
                            let rank = (GIT_SUBCOMMANDS.len() - idx) as i32;
                            let tie_boost = if prefix.is_empty() { rank } else { rank.min(18) };
                            scored.push((
                                score + 65 + tie_boost,
                                CompletionItem {
                                    label: sub.to_owned(),
                                    insert: sub.to_owned(),
                                    detail: "git".to_owned(),
                                    doc: desc.to_owned(),
                                    kind: CompletionKind::Subcommand,
                                    icon: None,
                                    avatar_path: None,
                                    fs_meta: None,
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
                                    avatar_path: None,
                                    fs_meta: None,
                                    match_indices: idxs,
                                },
                            ));
                        }
                    }
                    if matches!(subcmd, Some("switch" | "merge" | "rebase" | "branch")) && !scored.is_empty() {
                        handled_specialized = true;
                    }
                }
            } else if cmd_base == "bgit" {
                let active_spec = resolve_bgit_spec(&stage_words[1..]);
                if !active_spec.subcommands.is_empty() {
                    let total_subs = active_spec.subcommands.len();
                    for (idx, sub) in active_spec.subcommands.iter().enumerate() {
                        if sub.hidden {
                            continue;
                        }
                        if let Some((score, idxs)) = fuzzy_match_score(sub.name, prefix) {
                            let rank = (total_subs.saturating_sub(idx)).min(16) as i32;
                            scored.push((
                                score + 70 + rank,
                                CompletionItem {
                                    label: sub.name.to_owned(),
                                    insert: sub.name.to_owned(),
                                    detail: "bgit".to_owned(),
                                    doc: sub.about.to_owned(),
                                    kind: CompletionKind::Subcommand,
                                    icon: None,
                                    avatar_path: None,
                                    fs_meta: None,
                                    match_indices: idxs,
                                },
                            ));
                        }
                    }
                    handled_specialized = true;
                } else if matches!(active_spec.name, "merge" | "merge-base" | "log" | "add") {
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
                                    avatar_path: None,
                                    fs_meta: None,
                                    match_indices: idxs,
                                },
                            ));
                        }
                    }
                }
            } else if cmd_base == "blaze" && subcmd.is_none() {
                let total_cmds = blaze_cli_spec::blaze_app::BLAZE_APP.commands.len();
                for (idx, cmd) in blaze_cli_spec::blaze_app::BLAZE_APP.commands.iter().enumerate() {
                    if cmd.hidden {
                        continue;
                    }
                    if let Some((score, idxs)) = fuzzy_match_score(cmd.name, prefix) {
                        let rank = (total_cmds.saturating_sub(idx)).min(16) as i32;
                        scored.push((
                            score + 65 + rank,
                            CompletionItem {
                                label: cmd.name.to_owned(),
                                insert: cmd.name.to_owned(),
                                detail: "blaze".to_owned(),
                                doc: cmd.about.to_owned(),
                                kind: CompletionKind::Subcommand,
                                icon: Some("rust_dark"),
                                avatar_path: None,
                                fs_meta: None,
                                match_indices: idxs,
                            },
                        ));
                    }
                }
                handled_specialized = true;
            } else if subcmd.is_none()
                && !cmd_base.is_empty()
                && !matches!(cmd_base, "cd" | "z" | "zi" | "ls" | "open" | "cat" | "vim" | "nvim")
            {
                // Dynamic subcommand completion for ANY CLI (`claude`, `cargo`, `gh`, `docker`, `kubectl`, etc.)
                // via our `#![no_std]` `blaze-cli-spec` parser + user `[completions.<cmd>.subcommands]`.
                let home = state.get_env("HOME").unwrap_or("");
                let path_env = state
                    .get_env("PATH")
                    .map(str::to_owned)
                    .or_else(|| std::env::var("PATH").ok())
                    .unwrap_or_default();
                let mut seen_subs: HashSet<String> = HashSet::new();

                let user_cfg = crate::config::load_cached(home);
                if let Some(ov) = user_cfg.completion_override(cmd_base) {
                    for entry in ov.subcommands.iter() {
                        let sub = entry.name.as_str();
                        if let Some((score, idxs)) = fuzzy_match_score(sub, prefix)
                            && seen_subs.insert(sub.to_owned())
                        {
                            scored.push((
                                score + 70,
                                CompletionItem {
                                    label: sub.to_owned(),
                                    insert: sub.to_owned(),
                                    detail: cmd_base.to_owned(),
                                    doc: entry.desc.as_str().to_owned(),
                                    kind: CompletionKind::Subcommand,
                                    icon: None,
                                    avatar_path: None,
                                    fs_meta: None,
                                    match_indices: idxs,
                                },
                            ));
                        }
                    }
                }
                if let Some(spec) = crate::cli_spec_driver::lookup_nonblocking(
                    cmd_base,
                    None,
                    &path_env,
                    (!home.is_empty()).then_some(home),
                ) {
                    for s in spec.subcommands.iter() {
                        let sub = s.name.as_str();
                        if let Some((score, idxs)) = fuzzy_match_score(sub, prefix)
                            && seen_subs.insert(sub.to_owned())
                        {
                            scored.push((
                                score + 62,
                                CompletionItem {
                                    label: sub.to_owned(),
                                    insert: sub.to_owned(),
                                    detail: cmd_base.to_owned(),
                                    doc: s.description.as_str().to_owned(),
                                    kind: CompletionKind::Subcommand,
                                    icon: None,
                                    avatar_path: None,
                                    fs_meta: None,
                                    match_indices: idxs,
                                },
                            ));
                        }
                    }
                }
            }

            // Column-aware completion for table pipeline operators (`where`, `sort`, `sort-by`, `top`, `freq`, `select`, `get`, ...).
            if !handled_specialized && expects_column_arg(before) {
                let cols = infer_pipeline_columns(before, state, recent_cols);
                let mut added_col = false;
                for (col_name, col_ty) in cols {
                    if let Some((score, idxs)) = fuzzy_match_score(&col_name, prefix) {
                        added_col = true;
                        scored.push((
                            score + 55,
                            CompletionItem {
                                label: col_name.clone(),
                                insert: col_name.clone(),
                                detail: format!("col · {col_ty}"),
                                doc: format!("column `{col_name}` · {col_ty}"),
                                kind: CompletionKind::Column,
                                icon: None,
                                avatar_path: None,
                                fs_meta: None,
                                match_indices: idxs,
                            },
                        ));
                    }
                }
                if added_col {
                    handled_specialized = true;
                }
            }
            // Remote sans-IO repository & package search (`git clone`, `gh repo clone`, `cargo add`,
            // `bun add`, `npm install`, `brew install`) with live Retina GitHub org/owner avatars!
            if !handled_specialized
                && let Some(rctx) = blaze_cli_spec::remote::detect_remote_context(
                    before,
                    blaze_own::Alloc::global(),
                )
            {
                handled_specialized = true;
                let provider_tag = match rctx.provider {
                    blaze_cli_spec::remote::RemoteProvider::GitHubRepo { .. } => "GitHub",
                    blaze_cli_spec::remote::RemoteProvider::CratesIo => "crates.io",
                    blaze_cli_spec::remote::RemoteProvider::NpmRegistry => "npm",
                    blaze_cli_spec::remote::RemoteProvider::Homebrew => "Homebrew",
                };
                match crate::remote_driver::query_remote_completions(
                    rctx.provider,
                    rctx.query.as_str(),
                ) {
                    crate::remote_driver::RemotePoll::Idle => {
                        scored.push((
                            1000,
                            CompletionItem {
                                label: rctx.hint.to_owned(),
                                insert: String::new(),
                                detail: provider_tag.to_owned(),
                                doc: format!(
                                    "Live {provider_tag} search — type a repo or package name (e.g. ripgrep, owner/repo)"
                                ),
                                kind: CompletionKind::Hint,
                                icon: None,
                                avatar_path: None,
                                fs_meta: None,
                                match_indices: Vec::new(),
                            },
                        ));
                    }
                    crate::remote_driver::RemotePoll::Loading => {
                        scored.push((
                            1000,
                            CompletionItem {
                                label: format!("Searching {provider_tag} for \"{}\"…", rctx.query.as_str()),
                                insert: String::new(),
                                detail: "loading".to_owned(),
                                doc: format!("Querying {provider_tag} asynchronously (non-blocking)"),
                                kind: CompletionKind::Hint,
                                icon: None,
                                avatar_path: None,
                                fs_meta: None,
                                match_indices: Vec::new(),
                            },
                        ));
                    }
                    crate::remote_driver::RemotePoll::Ready(suggestions) => {
                        if suggestions.is_empty() {
                            scored.push((
                                1000,
                                CompletionItem {
                                    label: format!("No {provider_tag} matches for \"{}\"", rctx.query.as_str()),
                                    insert: String::new(),
                                    detail: provider_tag.to_owned(),
                                    doc: "Keep typing or check spelling".to_owned(),
                                    kind: CompletionKind::Hint,
                                    icon: None,
                                    avatar_path: None,
                                    fs_meta: None,
                                    match_indices: Vec::new(),
                                },
                            ));
                        } else {
                            for (rank, sug) in suggestions.into_iter().enumerate() {
                                let (kind, icon) = match sug.provider {
                                    blaze_cli_spec::remote::RemoteProvider::GitHubRepo { .. } => {
                                        (CompletionKind::RemoteRepo, None)
                                    }
                                    blaze_cli_spec::remote::RemoteProvider::CratesIo => {
                                        (CompletionKind::RemotePackage, Some("rust_dark"))
                                    }
                                    blaze_cli_spec::remote::RemoteProvider::NpmRegistry => {
                                        (CompletionKind::RemotePackage, Some("javascript_dark"))
                                    }
                                    blaze_cli_spec::remote::RemoteProvider::Homebrew => {
                                        (CompletionKind::RemotePackage, None)
                                    }
                                };
                                let match_indices = fuzzy_match_score(&sug.label, rctx.query.as_str())
                                    .map(|(_, idxs)| idxs)
                                    .unwrap_or_default();
                                scored.push((
                                    2000 - rank as i32,
                                    CompletionItem {
                                        label: sug.label,
                                        insert: sug.insert,
                                        detail: sug.detail,
                                        doc: sug.doc,
                                        kind,
                                        icon,
                                        avatar_path: sug.avatar_path,
                                        fs_meta: None,
                                        match_indices,
                                    },
                                ));
                            }
                        }
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
                    let anchored_prefix: Option<String> = if prefix.starts_with('/') {
                        Some(prefix.to_owned())
                    } else if let Some(rest) = prefix.strip_prefix("~/") {
                        Some(format!("{}/{rest}", home.trim_end_matches('/')))
                    } else if prefix == "~" && !home.is_empty() {
                        Some(home.to_owned())
                    } else {
                        None
                    };
                    let (prefix_dir_part, prefix_leaf) = match prefix.rfind('/') {
                        Some(idx) => (&prefix[..=idx], &prefix[idx + 1..]),
                        None => ("", prefix),
                    };
                    let resolved_dir_prefix: Option<String> = if prefix_dir_part.starts_with('/') {
                        Some(prefix_dir_part.to_owned())
                    } else if let Some(rest) = prefix_dir_part.strip_prefix("~/") {
                        Some(format!("{}/{rest}", home.trim_end_matches('/')))
                    } else {
                        None
                    };
                    for zm in self.zoxide.query(prefix, &self.local_host, now_ns, 20) {
                        if let Some(anchor) = anchored_prefix.as_deref()
                            && (!zm.is_local
                                || !zm
                                    .path
                                    .get(..anchor.len())
                                    .is_some_and(|p| p.eq_ignore_ascii_case(anchor)))
                        {
                            continue;
                        }
                        let short = crate::prompt::abbreviate_home(&zm.path, home);
                        let (label, match_query, insert, detail, doc, fs_meta) = if zm.is_local {
                            let dir_insert = if zm.path == "/" {
                                "/".to_owned()
                            } else {
                                format!("{}/", zm.path.trim_end_matches('/'))
                            };
                            let (disp_label, mq) = if let Some(dp) = resolved_dir_prefix.as_deref()
                                && dir_insert.len() > dp.len()
                                && dir_insert
                                    .get(..dp.len())
                                    .is_some_and(|p| p.eq_ignore_ascii_case(dp))
                            {
                                (dir_insert[dp.len()..].to_owned(), prefix_leaf)
                            } else {
                                (
                                    if short == "/" {
                                        "/".to_owned()
                                    } else {
                                        format!("{}/", short.trim_end_matches('/'))
                                    },
                                    prefix,
                                )
                            };
                            let mtime_ns = fs::metadata(&zm.path)
                                .ok()
                                .and_then(|m| m.modified().ok())
                                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                .map(|d| d.as_nanos().min(i64::MAX as u128) as i64);
                            (
                                disp_label,
                                mq,
                                shell_quote_if_needed(&dir_insert),
                                "recent".to_owned(),
                                format!("recent dir · {}", zm.path),
                                Some(CompletionFsMeta {
                                    path: zm.path.clone(),
                                    mtime_ns,
                                    size_bytes: None,
                                    is_direct_child: false,
                                }),
                            )
                        } else {
                            (
                                format!("{}:{}", zm.host, short),
                                prefix,
                                shell_quote_if_needed(&format!("{}:{}", zm.host, zm.path)),
                                format!("host · {}", zm.host),
                                format!("remote dir on {} · {}", zm.host, zm.path),
                                None,
                            )
                        };
                        let match_indices = fuzzy_match_score(&label, match_query)
                            .map(|(_, idxs)| idxs)
                            .unwrap_or_default();
                        scored.push((
                            // Rank distant zoxide directories strictly below direct local children
                            // (which score >= 60 even on long fuzzy names) so `cd rg` inside a
                            // folder containing `ripgrep/` always prioritizes the local folder!
                            ((zm.score as i32) / 20).clamp(10, 45),
                            CompletionItem {
                                label,
                                insert,
                                detail,
                                doc,
                                kind: CompletionKind::Dir,
                                icon: Some("folder_macos"),
                                avatar_path: None,
                                fs_meta,
                                match_indices,
                            },
                        ));
                    }
                }
                collect_path_completions(state.cwd.as_ref(), prefix, matches!(cmd_base, "cd" | "z"), &mut scored);
            }
        }

        // Boost candidates by recent command history frequency for the same command prefix,
        // AND ensure any item matching our Fish ghost prediction ranks #1 so ghost text and
        // the highlighted popup row NEVER disagree!
        let before_tok = &self.text[..token_start.min(self.cursor)];
        let ghost_predicted_tok: Option<String> = self.ghost.as_deref().and_then(|g| {
            let mut full = String::with_capacity(prefix.len() + g.len());
            full.push_str(prefix);
            full.push_str(g);
            full.split_whitespace().next().map(str::to_owned)
        });
        let mut hist_boosts: HashMap<String, i32> = HashMap::new();
        if !before_tok.is_empty() && !self.history.is_empty() {
            let total_hist = self.history.len();
            let start_idx = total_hist.saturating_sub(256);
            for (offset, cmd) in self.history[start_idx..].iter().enumerate() {
                if let Some(rest) = cmd.strip_prefix(before_tok)
                    && let Some(tok) = rest.split_whitespace().next()
                    && !tok.is_empty()
                {
                    let recency = 1 + (offset as i32 * 4) / (total_hist - start_idx).max(1) as i32;
                    let entry = hist_boosts.entry(tok.to_owned()).or_insert(0);
                    *entry = (*entry + recency).min(45);
                }
            }
        }
        for (score, item) in &mut scored {
            if let Some(&hb) = hist_boosts.get(&item.insert) {
                *score += hb;
            }
            if let Some(gt) = ghost_predicted_tok.as_deref()
                && item.kind != CompletionKind::Hint
                && item.insert == gt
            {
                *score += 120;
            }
        }

        // Highest match score first, then alphabetical by label.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.label.cmp(&b.1.label)));

        // Deduplicate by label AND (for filesystem/dir entries) by normalized insert target so
        // `Projects/` and zoxide `/Volumes/Projects/` never appear twice for the same folder.
        let mut seen_labels: HashSet<String> = HashSet::new();
        let mut seen_inserts: HashSet<String> = HashSet::new();
        let mut items: Vec<CompletionItem> = Vec::new();
        for (_, item) in scored {
            let dup_insert = matches!(item.kind, CompletionKind::Dir | CompletionKind::File)
                && !item.insert.is_empty()
                && !seen_inserts.insert(item.insert.clone());
            if !dup_insert && seen_labels.insert(item.label.clone()) {
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
        if item.kind == CompletionKind::Hint {
            return false;
        }
        self.push_undo(EditOp::Other);
        self.clamp_cursor();
        let start = clamp_char_boundary(&self.text, self.completion_replace_start).min(self.cursor);
        let mut insert = item.insert;
        let is_cell_path_insert = insert.starts_with('.')
            || insert.ends_with('.')
            || split_data_file_dot_prefix(&insert).is_some()
            || (insert.starts_with('$') && insert.contains('.'));
        if !is_cell_path_insert
            && matches!(
                item.kind,
                CompletionKind::Abbrev
                    | CompletionKind::Builtin
                    | CompletionKind::Subcommand
                    | CompletionKind::Def
                    | CompletionKind::Keyword
                    | CompletionKind::External
                    | CompletionKind::Column
            )
        {
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

/// Longest exact character prefix shared by every actionable (non-`Hint`) completion's `insert`.
fn longest_common_prefix(items: &[CompletionItem]) -> String {
    let mut iter = items
        .iter()
        .filter(|it| it.kind != CompletionKind::Hint && !it.insert.is_empty());
    let Some(first) = iter.next() else {
        return String::new();
    };
    let mut prefix: Vec<char> = first.insert.chars().collect();
    for item in iter {
        let mut common = 0usize;
        for (a, b) in prefix.iter().zip(item.insert.chars()) {
            if *a == b {
                common += 1;
            } else {
                break;
            }
        }
        prefix.truncate(common);
        if prefix.is_empty() {
            break;
        }
    }
    prefix.into_iter().collect()
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

    // Fast exact / exact-case prefix / case-insensitive prefix match gets top score.
    let prefix_exact = cand_chars
        .iter()
        .zip(query_chars.iter())
        .all(|(c, q)| c.eq_ignore_ascii_case(q));
    if prefix_exact {
        if candidate == query {
            return Some((340, (0..query_chars.len()).collect()));
        }
        let case_bonus = if candidate.starts_with(query) { 60 } else { 15 };
        let len_penalty = (cand_chars.len() - query_chars.len()).min(40) as i32;
        return Some((200 + case_bonus - len_penalty, (0..query_chars.len()).collect()));
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

fn is_inside_comment_or_quotes(before: &str) -> bool {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for ch in before.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if in_single {
            if ch == '\'' {
                in_single = false;
            }
            continue;
        }
        if in_double {
            if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_double = false;
            }
            continue;
        }
        match ch {
            '#' => return true,
            '\'' => in_single = true,
            '"' => in_double = true,
            _ => {}
        }
    }
    in_single || in_double
}

fn is_pure_number_token(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_digit() {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_hexdigit() || matches!(c, '.' | '_' | 'x' | 'X' | 'b' | 'B' | 'o' | 'O'))
}

fn infer_pipeline_columns(
    before_cursor: &str,
    state: &EngineState<'_>,
    recent_cols: &[String],
) -> Vec<(String, String)> {
    // 1. Live upstream structured value resolution (`Cargo.lock.package | sort `,
    //    `open Cargo.lock | .package | where `, `| sort ` continuing from `$_`)!
    if let Some(pipe_idx) = before_cursor.rfind('|') {
        let upstream = &before_cursor[..pipe_idx];
        let live_val = if upstream.trim().is_empty() {
            state.get_var("_").map(|s| clone_value_global(&s.value))
        } else {
            resolve_piped_value_for_dot(upstream, state)
        };
        if let Some(val) = live_val {
            match &val {
                Value::Table(t) => {
                    let mut cols = Vec::with_capacity(t.ncols());
                    for ci in 0..t.ncols() {
                        if let Some(name) = t.columns.get(ci) {
                            let ty = (0..t.nrows().min(16))
                                .find_map(|ri| {
                                    t.cell(ri, ci).and_then(|v| {
                                        (!matches!(v, Value::Nothing)).then(|| v.type_name())
                                    })
                                })
                                .unwrap_or("any");
                            cols.push((name.as_ref().to_owned(), ty.to_owned()));
                        }
                    }
                    if !cols.is_empty() {
                        return cols;
                    }
                }
                Value::Record(r) => {
                    let mut cols = Vec::new();
                    for (k, v) in r.iter() {
                        cols.push((k.to_owned(), v.type_name().to_owned()));
                    }
                    if !cols.is_empty() {
                        return cols;
                    }
                }
                _ => {}
            }
        }
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

/// Zero-Tab, non-modal IDE signature strip rendered directly below the active prompt line when
/// no completion candidate popup is open (e.g. `git add <pathspec>...` with clickable flag chips).
/// Subcommand & flag candidate lists are unified into [`Editor::completions`] so there is only
/// ONE popup with selection highlight, live ghost sync, and instant `Tab` completion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiveHint {
    /// Recognized command / subcommand signature + 1-line description + key clickable flag chips.
    Signature {
        /// Byte offset of the active token (for anchoring).
        anchor_byte: usize,
        /// Monospace usage signature (e.g. `git add <pathspec>...` or `bgit worktree add <PATH> <BRANCH>`).
        signature: String,
        /// Calm SF Pro description.
        about: String,
        /// Up to 4 compact clickable flag chips `(flag_token, short_desc)`.
        flags: Vec<(String, String)>,
    },
}

/// Walks `bgit`'s declarative `bstd::cli::CommandSpec` tree along non-flag tokens (`worktree`, `add`, …).
fn resolve_bgit_spec<'a>(words: &[&str]) -> &'static bgit::CliCommandSpec<'static> {
    let mut cur = bgit::cli_spec();
    for &w in words {
        if w.starts_with('-') {
            continue;
        }
        if let Some(sub) = cur
            .subcommands
            .iter()
            .find(|s| s.name == w || s.aliases.contains(&w))
        {
            cur = sub.spec;
        } else {
            break;
        }
    }
    cur
}

struct GitSubMeta {
    name: &'static str,
    usage: &'static str,
    about: &'static str,
    flags: &'static [(&'static str, &'static str, &'static str)],
}

const GIT_SUBCOMMAND_SIGNATURES: &[GitSubMeta] = &[
    GitSubMeta {
        name: "status",
        usage: "git status [options]",
        about: "Show working tree status",
        flags: &[
            ("-s", "short", "Give output in short format"),
            ("-sb", "short+branch", "Short format with branch header"),
            ("--porcelain", "machine", "Machine-readable v1 output"),
            ("-uno", "no untracked", "Hide untracked files"),
        ],
    },
    GitSubMeta {
        name: "add",
        usage: "git add <pathspec>...",
        about: "Stage file contents to the index",
        flags: &[
            ("-A", "all", "Stage all new, modified, and deleted files"),
            ("-p", "patch", "Interactively choose hunks to stage"),
            ("-u", "tracked", "Stage modified and deleted tracked files"),
            ("-n", "dry-run", "Show what would be staged without staging"),
        ],
    },
    GitSubMeta {
        name: "commit",
        usage: "git commit [options]",
        about: "Record staged changes to the repository",
        flags: &[
            ("-m", "message", "Use the given commit message"),
            ("--amend", "amend", "Amend the previous commit"),
            ("-a", "all tracked", "Auto-stage modified and deleted files"),
            ("--no-edit", "keep msg", "Reuse existing message when amending"),
        ],
    },
    GitSubMeta {
        name: "diff",
        usage: "git diff [commit] [-- <path>...]",
        about: "Show changes between commits or working tree",
        flags: &[
            ("--staged", "staged", "Show diff of staged changes"),
            ("--stat", "diffstat", "Show file-level diffstat summary"),
            ("--name-only", "names", "Show only names of changed files"),
            ("-w", "no ws", "Ignore whitespace changes"),
        ],
    },
    GitSubMeta {
        name: "log",
        usage: "git log [options] [<rev>]",
        about: "Show commit history",
        flags: &[
            ("--oneline", "compact", "One line per commit (short SHA + subject)"),
            ("-n", "limit", "Limit number of commits shown"),
            ("--graph", "graph", "Draw ASCII branch/merge graph"),
            ("--stat", "files", "Include changed file statistics"),
        ],
    },
    GitSubMeta {
        name: "switch",
        usage: "git switch <branch>",
        about: "Switch to a branch",
        flags: &[
            ("-c", "create", "Create and switch to a new branch"),
            ("-C", "reset", "Create or reset a branch and switch to it"),
            ("-", "previous", "Switch back to the previous branch"),
        ],
    },
    GitSubMeta {
        name: "checkout",
        usage: "git checkout <branch|commit>",
        about: "Switch branches or restore working tree files",
        flags: &[
            ("-b", "new branch", "Create and check out a new branch"),
            ("-B", "reset branch", "Create or reset branch and check out"),
            ("-f", "force", "Discard local changes when switching"),
        ],
    },
    GitSubMeta {
        name: "branch",
        usage: "git branch [options] [<name>]",
        about: "List, create, or delete branches",
        flags: &[
            ("-a", "all", "List local and remote-tracking branches"),
            ("-d", "delete", "Delete a merged branch"),
            ("-D", "force del", "Force-delete a branch"),
            ("-m", "rename", "Rename a branch"),
        ],
    },
    GitSubMeta {
        name: "push",
        usage: "git push [<remote> [<refspec>]]",
        about: "Update remote refs along with associated objects",
        flags: &[
            ("-u", "upstream", "Set upstream tracking branch"),
            ("--force-with-lease", "safe force", "Force push only if remote matches expected tip"),
            ("--tags", "tags", "Push all tags"),
            ("--dry-run", "dry-run", "Simulate push without updating remote"),
        ],
    },
    GitSubMeta {
        name: "pull",
        usage: "git pull [<remote> [<branch>]]",
        about: "Fetch from and integrate with another repository",
        flags: &[
            ("--rebase", "rebase", "Rebase local commits onto upstream"),
            ("--ff-only", "ff only", "Refuse to merge unless fast-forward"),
            ("--autostash", "autostash", "Automatically stash/pop dirty tree"),
        ],
    },
    GitSubMeta {
        name: "fetch",
        usage: "git fetch [<remote>]",
        about: "Download objects and refs from another repository",
        flags: &[
            ("--all", "all remotes", "Fetch all configured remotes"),
            ("--prune", "prune", "Remove deleted remote-tracking refs"),
            ("--tags", "tags", "Fetch all tags from remote"),
        ],
    },
    GitSubMeta {
        name: "rebase",
        usage: "git rebase [options] <upstream>",
        about: "Reapply commits on top of another base tip",
        flags: &[
            ("-i", "interactive", "Interactively edit/squash/reorder commits"),
            ("--continue", "continue", "Continue rebase after resolving conflicts"),
            ("--abort", "abort", "Abort rebase and restore original branch"),
            ("--autostash", "autostash", "Stash and re-apply dirty changes"),
        ],
    },
    GitSubMeta {
        name: "stash",
        usage: "git stash [push|pop|apply|list|drop]",
        about: "Stash changes in a dirty working directory",
        flags: &[
            ("-u", "untracked", "Include untracked files in stash"),
            ("-m", "message", "Attach a description to the stash"),
            ("-p", "patch", "Interactively select hunks to stash"),
        ],
    },
    GitSubMeta {
        name: "merge",
        usage: "git merge [options] <branch>",
        about: "Join two or more development histories together",
        flags: &[
            ("--squash", "squash", "Squash all commits into staged changes"),
            ("--no-ff", "merge commit", "Always create a merge commit"),
            ("--ff-only", "ff only", "Abort if fast-forward is not possible"),
            ("--abort", "abort", "Abort an in-progress conflicted merge"),
        ],
    },
    GitSubMeta {
        name: "restore",
        usage: "git restore [options] <pathspec>...",
        about: "Restore working tree or staged files",
        flags: &[
            ("--staged", "unstage", "Restore index (unstage files)"),
            ("-s", "source", "Restore from a specific commit or branch"),
            ("-p", "patch", "Interactively discard hunks"),
        ],
    },
    GitSubMeta {
        name: "reset",
        usage: "git reset [--soft|--mixed|--hard] [<commit>]",
        about: "Reset current HEAD to the specified state",
        flags: &[
            ("--soft", "keep staged", "Move HEAD only, keep index & working tree"),
            ("--mixed", "unstage", "Move HEAD and reset index (default)"),
            ("--hard", "discard all", "Reset HEAD, index, and working tree"),
        ],
    },
    GitSubMeta {
        name: "show",
        usage: "git show [<object>]",
        about: "Show commit metadata and diff for an object",
        flags: &[
            ("--stat", "diffstat", "Show changed file summary only"),
            ("--name-only", "names", "Show only changed file names"),
        ],
    },
    GitSubMeta {
        name: "cherry-pick",
        usage: "git cherry-pick <commit>...",
        about: "Apply changes introduced by existing commits",
        flags: &[
            ("-n", "no commit", "Apply changes without committing"),
            ("--continue", "continue", "Continue after resolving conflicts"),
            ("--abort", "abort", "Abort cherry-pick and restore state"),
        ],
    },
    GitSubMeta {
        name: "worktree",
        usage: "git worktree <add|list|remove|prune>",
        about: "Manage multiple working trees (auto-merging via bgit when infected)",
        flags: &[
            ("--track", "auto-merge", "Track upstream branch for auto-merge"),
            ("--vfs", "lazy VFS", "Mount worktree lazily via NFSv3 CoW"),
            ("--force", "force", "Force checkout or removal"),
        ],
    },
];

impl Editor {
    /// Replace `[replace_start..cursor]` with `insert` (appending a trailing space when inserting a
    /// subcommand or flag) when the user clicks an item in the zero-Tab live hint strip.
    pub fn apply_live_hint(&mut self, replace_start: usize, insert: &str) {
        let start = replace_start.min(self.cursor).min(self.text.len());
        let end = self.cursor.min(self.text.len());
        let mut replacement = insert.to_owned();
        if !replacement.ends_with(' ') && !replacement.ends_with('=') && !replacement.ends_with('/') {
            replacement.push(' ');
        }
        self.text.replace_range(start..end, &replacement);
        self.cursor = start + replacement.len();
        self.selection_anchor = None;
        self.completions.clear();
        self.ghost = None;
    }

    /// Computes a zero-Tab, non-modal live inline hint for the current command under the cursor
    /// (`bgit`, `git`, `blaze`, `blaze-sh` builtins, and all `blaze-cli-spec::manual` CLIs).
    pub fn live_inline_hint(&self) -> Option<LiveHint> {
        if !self.completions.is_empty()
            || self.reverse_search.is_some()
            || self.vim_mode() != VimMode::Insert
            || self.text.contains('\n')
        {
            return None;
        }
        let cursor = self.cursor.min(self.text.len());
        let before = &self.text[..cursor];
        let stage_start = before.rfind(['|', ';', '\n']).map_or(0, |p| p + 1);
        let stage_raw = &before[stage_start..];
        let stage_trimmed = stage_raw.trim_start();
        if stage_trimmed.is_empty() {
            return None;
        }

        let trailing_space = stage_raw.ends_with(char::is_whitespace);
        let words: Vec<&str> = stage_trimmed.split_whitespace().collect();
        if words.is_empty() {
            return None;
        }

        // Only show live hints once a command word is followed by whitespace (at least 1 arg slot).
        if words.len() == 1 && !trailing_space {
            return None;
        }

        let cmd_base = words[0].trim_start_matches('^').rsplit('/').next().unwrap_or(words[0]);
        let (done_words, cur_prefix, replace_start) = if trailing_space {
            (&words[1..], "", cursor)
        } else {
            let last = *words.last().unwrap_or(&"");
            let rstart = cursor.saturating_sub(last.len());
            (&words[1..words.len() - 1], last, rstart)
        };
        let typing_flag = cur_prefix.starts_with('-');

        // 1. `bgit` — driven directly from `bgit::cli_spec()` (supports nested `worktree add/list/sync/...`!).
        if cmd_base == "bgit" {
            if typing_flag {
                return None;
            }
            let active = resolve_bgit_spec(done_words);
            if !active.subcommands.is_empty() {
                if !cur_prefix.is_empty()
                    && let Some(exact) = active
                        .subcommands
                        .iter()
                        .find(|s| !s.hidden && (s.name == cur_prefix || s.aliases.contains(&cur_prefix)))
                {
                    let matching_count = active
                        .subcommands
                        .iter()
                        .filter(|s| !s.hidden && fuzzy_match_score(s.name, cur_prefix).is_some())
                        .count();
                    if matching_count <= 1 && exact.spec.subcommands.is_empty() {
                        return Some(build_bgit_signature(exact.spec, done_words, replace_start));
                    }
                }
                return None;
            }
            return Some(build_bgit_signature(active, done_words, replace_start));
        }

        // 2. `git` — curated rich subcommand signatures.
        if cmd_base == "git" {
            if typing_flag {
                return None;
            }
            let sub = done_words.iter().copied().find(|w| !w.starts_with('-'));
            if let Some(sub_name) = sub {
                if let Some(meta) = GIT_SUBCOMMAND_SIGNATURES.iter().find(|m| m.name == sub_name) {
                    return Some(LiveHint::Signature {
                        anchor_byte: replace_start,
                        signature: meta.usage.to_owned(),
                        about: meta.about.to_owned(),
                        flags: meta
                            .flags
                            .iter()
                            .take(4)
                            .map(|&(f, short, _)| (f.to_owned(), short.to_owned()))
                            .collect(),
                    });
                }
                return None;
            }
            if !cur_prefix.is_empty()
                && let Some(meta) = GIT_SUBCOMMAND_SIGNATURES.iter().find(|m| m.name == cur_prefix)
            {
                let matching_count = GIT_SUBCOMMANDS
                    .iter()
                    .filter(|&&(name, _)| name.starts_with(cur_prefix))
                    .count();
                if matching_count <= 1 {
                    return Some(LiveHint::Signature {
                        anchor_byte: replace_start,
                        signature: meta.usage.to_owned(),
                        about: meta.about.to_owned(),
                        flags: meta
                            .flags
                            .iter()
                            .take(4)
                            .map(|&(f, short, _)| (f.to_owned(), short.to_owned()))
                            .collect(),
                    });
                }
            }
            return None;
        }

        // 3. `blaze` — driven directly from `blaze_cli_spec::blaze_app::BLAZE_APP`.
        if cmd_base == "blaze" {
            if typing_flag {
                return None;
            }
            let app = &blaze_cli_spec::blaze_app::BLAZE_APP;
            let sub = done_words.iter().copied().find(|w| !w.starts_with('-'));
            if let Some(sub_name) = sub
                && let Some(cmd) = blaze_cli_spec::app::find_command(app, sub_name)
            {
                let sig = if cmd.usage_args.is_empty() {
                    format!("blaze {}", cmd.name)
                } else {
                    format!("blaze {} {}", cmd.name, cmd.usage_args)
                };
                let flags = cmd
                    .flags
                    .iter()
                    .filter(|f| !f.hidden)
                    .take(4)
                    .filter_map(|f| {
                        let tok = f
                            .long
                            .map(|l| format!("--{l}"))
                            .or_else(|| f.short.map(|c| format!("-{c}")))?;
                        Some((tok, compact_flag_label(f.about)))
                    })
                    .collect();
                return Some(LiveHint::Signature {
                    anchor_byte: replace_start,
                    signature: sig,
                    about: cmd.about.to_owned(),
                    flags,
                });
            }
            if !cur_prefix.is_empty()
                && let Some(cmd) = blaze_cli_spec::app::find_command(app, cur_prefix)
            {
                let n_match = app
                    .commands
                    .iter()
                    .filter(|c| !c.hidden && c.name.starts_with(cur_prefix))
                    .count();
                if n_match <= 1 {
                    let sig = if cmd.usage_args.is_empty() {
                        format!("blaze {}", cmd.name)
                    } else {
                        format!("blaze {} {}", cmd.name, cmd.usage_args)
                    };
                    return Some(LiveHint::Signature {
                        anchor_byte: replace_start,
                        signature: sig,
                        about: cmd.about.to_owned(),
                        flags: cmd
                            .flags
                            .iter()
                            .filter(|f| !f.hidden)
                            .take(4)
                            .filter_map(|f| {
                                let tok = f
                                    .long
                                    .map(|l| format!("--{l}"))
                                    .or_else(|| f.short.map(|c| format!("-{c}")))?;
                                Some((tok, compact_flag_label(f.about)))
                            })
                            .collect(),
                    });
                }
            }
            return None;
        }

        // 4. Native `blaze-sh` builtins (`ls`, `where`, `sort-by`, `select`, `ps`, `rg`, …).
        if let Some(spec) = find_spec(cmd_base) {
            if typing_flag {
                return None;
            }
            return Some(LiveHint::Signature {
                anchor_byte: replace_start,
                signature: spec.signature.to_owned(),
                about: spec.summary.to_owned(),
                flags: spec
                    .flags
                    .iter()
                    .take(4)
                    .map(|&f| (f.to_owned(), String::new()))
                    .collect(),
            });
        }

        // 5. Any curated CLI in `blaze-cli-spec::manual` (`cargo`, `claude`, `rg`, `fd`, `curl`, `ssh`, `vim`, …).
        if typing_flag {
            return None;
        }
        let alloc = Alloc::global();
        let sub = done_words.iter().copied().find(|w| !w.starts_with('-'));
        if let Some(cli) = sub
            .and_then(|s| blaze_cli_spec::manual::manual_spec_in(cmd_base, Some(s), alloc))
            .or_else(|| blaze_cli_spec::manual::manual_spec_in(cmd_base, None, alloc))
        {
            if let Some(sub_name) = sub {
                if let Some(sc) = cli.subcommands.iter().find(|s| s.name.as_str() == sub_name) {
                    return Some(LiveHint::Signature {
                        anchor_byte: replace_start,
                        signature: format!("{cmd_base} {}", sc.name.as_str()),
                        about: sc.description.as_str().to_owned(),
                        flags: cli
                            .flags
                            .iter()
                            .take(3)
                            .filter(|f| !f.primary().is_empty())
                            .map(|f| (f.primary().to_owned(), compact_flag_label(f.description.as_str())))
                            .collect(),
                    });
                }
                return None;
            }
            if !cli.subcommands.is_empty()
                && !cur_prefix.is_empty()
                && let Some(sc) = cli.subcommands.iter().find(|s| s.name.as_str() == cur_prefix)
            {
                let n_match = cli
                    .subcommands
                    .iter()
                    .filter(|s| s.name.as_str().starts_with(cur_prefix))
                    .count();
                if n_match <= 1 {
                    return Some(LiveHint::Signature {
                        anchor_byte: replace_start,
                        signature: format!("{cmd_base} {}", sc.name.as_str()),
                        about: sc.description.as_str().to_owned(),
                        flags: Vec::new(),
                    });
                }
            }
        }

        None
    }
}

fn compact_flag_label(desc: &str) -> String {
    let words: Vec<&str> = desc.split_whitespace().take(3).collect();
    words.join(" ")
}

fn build_bgit_signature(
    spec: &bgit::CliCommandSpec<'static>,
    done_words: &[&str],
    anchor_byte: usize,
) -> LiveHint {
    let mut sig = String::from("bgit");
    for &w in done_words {
        if !w.starts_with('-') && w != spec.name {
            sig.push(' ');
            sig.push_str(w);
        }
    }
    if spec.name != "bgit" {
        sig.push(' ');
        sig.push_str(spec.name);
    }
    for pos in spec.positionals {
        sig.push(' ');
        if pos.required {
            sig.push('<');
            sig.push_str(pos.name);
            sig.push('>');
        } else {
            sig.push('[');
            sig.push_str(pos.name);
            sig.push(']');
        }
    }
    let mut flags = Vec::new();
    for f in spec.flags.iter().filter(|f| !f.hidden).take(4) {
        let tok = match f.short {
            Some(c) => format!("-{c}"),
            None => format!("--{}", f.long),
        };
        flags.push((tok, compact_flag_label(f.help)));
    }
    for o in spec.opts.iter().filter(|o| !o.hidden) {
        if flags.len() >= 4 {
            break;
        }
        flags.push((format!("--{}", o.long), o.value_name.to_lowercase()));
    }
    LiveHint::Signature { anchor_byte, signature: sig, about: spec.about.to_owned(), flags }
}

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
        let mtime_ns = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos().min(i64::MAX as u128) as i64);
        let size_bytes = if is_dir { None } else { meta.as_ref().map(|m| m.len()) };
        let full_fs_path = entry.path().to_string_lossy().into_owned();
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
        } else if let Some(sz) = size_bytes {
            crate::view::format_value_cell(&blaze_sh::Value::Size(Bytes(
                sz.min(i64::MAX as u64) as i64,
            )))
        } else {
            "file".to_owned()
        };
        let doc = if dir_part.is_empty() {
            format!("{cwd}/{label}")
        } else {
            raw_insert
        };
        // Direct children of the searched directory always outrank distant zoxide entries (<= 45)
        // while still staying below specialized column/subcommand completions on empty prefixes.
        let local_boost = if is_dir { 30 } else { 25 };
        out.push((
            score + local_boost,
            CompletionItem {
                label,
                insert,
                detail,
                doc,
                kind: if is_dir { CompletionKind::Dir } else { CompletionKind::File },
                icon,
                avatar_path: None,
                fs_meta: Some(CompletionFsMeta {
                    path: full_fs_path,
                    mtime_ns,
                    size_bytes,
                    is_direct_child: true,
                }),
                match_indices: idxs,
            },
        ));
    }

    // Lower-priority recursive git-tracked subfolder completion when inside a git repo:
    // surfaces nested files like `src/main.rs` (e.g. `vim .r` or `vim main`) cleanly beneath
    // direct `cwd` matches without scanning ignored build artifacts (`target/`, `node_modules/`).
    if !file_prefix.is_empty()
        && !dir_part.starts_with('/')
        && let Some(subpaths) = crate::prompt::git_tracked_subpaths(&search_dir, dirs_only, 8192)
    {
        let mut sub_hits: Vec<(i32, CompletionItem)> = Vec::new();
        for rel in subpaths {
            let trimmed = rel.trim_end_matches('/');
            let is_dir = rel.ends_with('/');
            if !show_hidden && trimmed.split('/').any(|seg| seg.starts_with('.')) {
                continue;
            }
            let (leaf_byte_off, leaf) = match trimmed.rfind('/') {
                Some(i) => (i + 1, &trimmed[i + 1..]),
                None => (0, trimmed),
            };
            let matched = if let Some((sc, mut leaf_idxs)) = fuzzy_match_score(leaf, file_prefix) {
                let char_off = rel[..leaf_byte_off].chars().count();
                for ix in &mut leaf_idxs {
                    *ix += char_off;
                }
                Some((sc + 18, leaf_idxs))
            } else {
                fuzzy_match_score(&rel, file_prefix)
            };
            let Some((raw_score, idxs)) = matched else {
                continue;
            };
            let depth = trimmed.bytes().filter(|&b| b == b'/').count() as i32;
            // Rank strictly below direct `cwd` matches; shallower subfolders outrank deeper ones.
            let adj_score = raw_score - 95 - depth * 15;
            let kind_str = if is_dir { Some("dir") } else { Some("file") };
            let icon = Some(icon_name_for_file(leaf, kind_str));
            let raw_insert = format!("{dir_part}{rel}");
            let insert = shell_quote_if_needed(&raw_insert);
            let parent_hint = if leaf_byte_off > 0 {
                &trimmed[..leaf_byte_off - 1]
            } else {
                ""
            };
            let detail = if is_dir {
                "git dir".to_owned()
            } else if parent_hint.is_empty() {
                "git".to_owned()
            } else {
                format!("git · {parent_hint}")
            };
            let doc = if dir_part.is_empty() {
                format!("{cwd}/{rel}")
            } else {
                raw_insert
            };
            sub_hits.push((
                adj_score,
                CompletionItem {
                    label: rel,
                    insert,
                    detail,
                    doc,
                    kind: if is_dir { CompletionKind::Dir } else { CompletionKind::File },
                    icon,
                    avatar_path: None,
                    fs_meta: None,
                    match_indices: idxs,
                },
            ));
        }
        sub_hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.label.cmp(&b.1.label)));
        sub_hits.truncate(20);
        for (_, item) in &mut sub_hits {
            let full = search_dir.join(item.label.trim_end_matches('/'));
            let meta = fs::metadata(&full).ok();
            let mtime_ns = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos().min(i64::MAX as u128) as i64);
            let size_bytes = if item.kind == CompletionKind::Dir {
                None
            } else {
                meta.as_ref().map(|m| m.len())
            };
            item.fs_meta = Some(CompletionFsMeta {
                path: full.to_string_lossy().into_owned(),
                mtime_ns,
                size_bytes,
                is_direct_child: false,
            });
        }
        out.extend(sub_hits);
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

/// Resolve the structured [`Value`] produced by pipeline stage(s) preceding `|` so typing
/// `open Cargo.lock | .pack`, `$lock | .pack`, or `open Cargo.lock | .package | .ver` gives
/// instant zero-Tab field/column completions with live value previews!
fn resolve_piped_value_for_dot(
    before_pipe: &str,
    state: &EngineState<'_>,
) -> Option<Value<'static>> {
    let stmt_start = before_pipe.rfind([';', '\n']).map_or(0, |p| p + 1);
    let stages: Vec<&str> = before_pipe[stmt_start..]
        .split('|')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let first = *stages.first()?;

    let mut cur = resolve_stage0_value(first, state)?;
    for &stage in &stages[1..] {
        if let Some(rest) = stage.strip_prefix('.') {
            cur = walk_dot_steps(&cur, rest)?;
        } else {
            let mut parts = stage.split_whitespace();
            let head = parts.next().unwrap_or("");
            match head {
                "first" => {
                    cur = match &cur {
                        Value::Table(t) => t.row_record(0).map(Value::Record)?,
                        Value::List(l) => l.get(0).map(Share::share)?,
                        _ => return None,
                    };
                }
                "last" => {
                    cur = match &cur {
                        Value::Table(t) => {
                            let idx = t.nrows().checked_sub(1)?;
                            t.row_record(idx).map(Value::Record)?
                        }
                        Value::List(l) => {
                            let idx = l.len().checked_sub(1)?;
                            l.get(idx).map(Share::share)?
                        }
                        _ => return None,
                    };
                }
                "get" => {
                    let key = parts.next()?.trim_start_matches('.').trim_matches(['"', '\'']);
                    cur = walk_dot_steps(&cur, key)?;
                }
                "schema" | "describe" => {
                    cur = blaze_sh::schema::infer_schema_value(&cur, Alloc::global());
                }
                "where" | "sort-by" | "reverse" | "skip" | "uniq" => {
                    // Preserves table/list schema unchanged!
                }
                _ => return None,
            }
        }
    }
    Some(cur)
}

fn resolve_stage0_value(stage: &str, state: &EngineState<'_>) -> Option<Value<'static>> {
    if stage == "." {
        return state.get_var("_").map(|s| clone_value_global(&s.value));
    }
    if let Some(rest) = stage.strip_prefix('.')
        && !stage.starts_with("./")
        && !stage.starts_with("..")
    {
        let root = state.get_var("_")?;
        let owned = clone_value_global(&root.value);
        return walk_dot_steps(&owned, rest);
    }
    if let Some(var_rest) = stage.strip_prefix('$') {
        let (var_name, dot_steps) = match var_rest.split_once('.') {
            Some((v, r)) => (v, Some(r)),
            None => (var_rest, None),
        };
        let base = if var_name == "env" {
            Value::Record(state.env_record())
        } else {
            state.get_var(var_name)?.value.share()
        };
        let owned = clone_value_global(&base);
        return match dot_steps {
            Some(steps) => walk_dot_steps(&owned, steps),
            None => Some(owned),
        };
    }

    // `Cargo.lock` or `Cargo.lock.package` bare structured file stage!
    if let Some((file_part, rest)) = split_data_file_dot_prefix(stage) {
        let abs = resolve_preview_path(state.cwd.as_ref(), file_part, state.get_env("HOME"));
        let parsed = load_cached_open_preview(&abs)?;
        return if rest.is_empty() {
            Some(parsed)
        } else {
            walk_dot_steps(&parsed, rest)
        };
    }
    if is_bare_data_file(stage) {
        let abs = resolve_preview_path(state.cwd.as_ref(), stage, state.get_env("HOME"));
        return load_cached_open_preview(&abs);
    }

    // `open <file>` -> parse structured file with mtime-keyed thread-local cache (<10us on hits!).
    let mut toks = stage.split_whitespace();
    let head = toks.next()?;
    if head == "open" {
        let mut raw_path: Option<&str> = None;
        for tok in toks {
            if tok == "-r" || tok == "--raw" {
                return None;
            }
            if !tok.starts_with('-') {
                raw_path = Some(tok.trim_matches(['"', '\'']));
                break;
            }
        }
        let rel = raw_path?;
        let abs = resolve_preview_path(state.cwd.as_ref(), rel, state.get_env("HOME"));
        return load_cached_open_preview(&abs);
    }
    None
}

fn clone_value_global(val: &Value<'_>) -> Value<'static> {
    let alloc = Alloc::global();
    match val {
        Value::Nothing => Value::Nothing,
        Value::Bool(b) => Value::Bool(*b),
        Value::Int(n) => Value::Int(*n),
        Value::Float(f) => Value::Float(*f),
        Value::Size(b) => Value::Size(*b),
        Value::Duration(d) => Value::Duration(*d),
        Value::Date(d) => Value::Date(*d),
        Value::String(s) => Value::string(s.as_ref(), alloc),
        Value::Binary(b) => Value::binary(b.as_ref(), alloc),
        Value::Record(r) => {
            let mut out = blaze_sh::Record::new(alloc);
            for (k, v) in r.iter() {
                out.insert(blaze_own::Rc::from_str_in(k, alloc), clone_value_global(v));
            }
            Value::Record(out)
        }
        Value::List(items) => {
            let mut out = blaze_sh::PVec::new(alloc);
            for it in items.iter().take(256) {
                out.push(clone_value_global(it));
            }
            Value::List(out)
        }
        Value::Table(t) => {
            let mut cols = blaze_sh::PVec::new(alloc);
            for ci in 0..t.ncols() {
                cols.push(blaze_own::Rc::from_str_in(t.columns.get(ci).unwrap().as_ref(), alloc));
            }
            let mut out = blaze_sh::Table::new(cols, alloc);
            let limit = t.nrows().min(128);
            for ri in 0..limit {
                let mut row_vals = Vec::with_capacity(t.ncols());
                for ci in 0..t.ncols() {
                    let cell = t.cell(ri, ci).map(clone_value_global).unwrap_or(Value::Nothing);
                    row_vals.push(cell);
                }
                out.push_row_values(&row_vals);
            }
            Value::Table(out)
        }
        _ => Value::Nothing,
    }
}

fn walk_dot_steps(root: &Value<'static>, dotted: &str) -> Option<Value<'static>> {
    let mut cur = root.share();
    for seg in dotted.split('.') {
        if seg.is_empty() {
            continue;
        }
        let clean = seg.trim_end_matches('?');
        let next = match &cur {
            Value::Record(r) => r.get(clean).map(Share::share),
            Value::Table(t) => {
                if let Ok(idx) = clean.parse::<usize>() {
                    t.row_record(idx).map(Value::Record)
                } else {
                    t.col_index(clean)
                        .and_then(|ci| t.cols.get(ci))
                        .map(|c| Value::List(c.share()))
                }
            }
            Value::List(items) => {
                if let Ok(idx) = clean.parse::<usize>() {
                    items.get(idx).map(Share::share)
                } else {
                    items.iter().find_map(|it| match it {
                        Value::Record(r) => r.get(clean).map(Share::share),
                        _ => None,
                    })
                }
            }
            Value::Output(o) => match clean {
                "stdout" => Some(o.stdout_value(Alloc::global())),
                "stderr" => Some(o.stderr_value(Alloc::global())),
                "exit_code" | "status" => Some(Value::Int(i64::from(o.exit_code))),
                "duration" => Some(Value::Duration(o.duration)),
                "events" | "chunks" | "streams" => Some(Value::Table(o.events_table(Alloc::global()))),
                _ => None,
            },
            _ => None,
        };
        cur = next?;
    }
    Some(cur)
}

fn split_data_file_dot_prefix(tok: &str) -> Option<(&str, &str)> {
    const EXTS: &[&str] = &[".toml.", ".lock.", ".json.", ".jsonl.", ".ndjson.", ".csv.", ".tsv."];
    for &ext in EXTS {
        if let Some(pos) = tok.find(ext) {
            let file_end = pos + ext.len() - 1;
            return Some((&tok[..file_end], &tok[file_end + 1..]));
        }
    }
    None
}

fn is_bare_data_file(tok: &str) -> bool {
    !tok.contains(char::is_whitespace)
        && [".toml", ".lock", ".json", ".jsonl", ".ndjson", ".csv", ".tsv"]
            .iter()
            .any(|ext| tok.ends_with(ext))
}

fn resolve_preview_path(cwd: &str, raw: &str, home: Option<&str>) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(h) = home
    {
        return PathBuf::from(h).join(rest);
    }
    let p = Path::new(raw);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        Path::new(cwd).join(p)
    }
}

fn load_cached_open_preview(abs: &Path) -> Option<Value<'static>> {
    use std::cell::RefCell;
    use std::time::UNIX_EPOCH;

    const MAX_PREVIEW_BYTES: u64 = 4 * 1024 * 1024;
    let meta = fs::metadata(abs).ok()?;
    if !meta.is_file() || meta.len() > MAX_PREVIEW_BYTES {
        return None;
    }
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let key = (abs.to_string_lossy().into_owned(), mtime_ns, meta.len());

    thread_local! {
        static CACHE: RefCell<Vec<((String, u128, u64), Option<Value<'static>>)>> =
            const { RefCell::new(Vec::new()) };
    }

    CACHE.with(|cell| {
        let mut entries = cell.borrow_mut();
        if let Some((_, cached)) = entries.iter().find(|(k, _)| *k == key) {
            return cached.as_ref().map(Share::share);
        }
        let bytes = fs::read(abs).ok()?;
        let parsed = blaze_sh::builtins::parse_open_preview(&key.0, &bytes, Alloc::global());
        if entries.len() >= 16 {
            entries.remove(0);
        }
        let out = parsed.as_ref().map(Share::share);
        entries.push((key, parsed));
        out
    })
}

/// Fallback schema completions for `ls | .` / `ps | .` / `tree | .` when the upstream command
/// hasn't been executed yet.
fn collect_piped_command_schema_dot_completions(
    before_pipe: &str,
    leaf_prefix: &str,
    scored: &mut Vec<(i32, CompletionItem)>,
) {
    if leaf_prefix.contains('.') {
        return;
    }
    let last_stage = before_pipe.rsplit(['|', ';', '\n']).next().unwrap_or("").trim();
    let head = last_stage.split_whitespace().next().unwrap_or("");
    let cols: &[(&str, &str, &str)] = match head {
        "ls" | "ll" | "la" => &[
            ("name", "string", "entry filename"),
            ("type", "string", "file / dir / symlink"),
            ("size", "size", "byte size"),
            ("modified", "date", "last modified timestamp"),
            ("git_status", "string", "git working-tree status"),
        ],
        "tree" => &[
            ("name", "string", "entry name"),
            ("size", "size", "byte size"),
            ("modified", "date", "last modified timestamp"),
            ("path", "string", "relative path"),
            ("depth", "int", "tree nesting depth"),
        ],
        "ps" => &[
            ("pid", "int", "process id"),
            ("name", "string", "process name"),
            ("cpu", "float", "CPU usage %"),
            ("mem", "size", "resident memory"),
            ("status", "string", "process state"),
        ],
        _ => return,
    };
    for (idx, &(name, ty, doc)) in cols.iter().enumerate() {
        if let Some((score, idxs)) = fuzzy_match_score(name, leaf_prefix) {
            scored.push((
                score + 80 + (cols.len() - idx) as i32,
                CompletionItem {
                    label: name.to_owned(),
                    insert: format!(".{name}"),
                    detail: ty.to_owned(),
                    doc: doc.to_owned(),
                    kind: CompletionKind::Column,
                    icon: None,
                    avatar_path: None,
                    fs_meta: None,
                    match_indices: idxs,
                },
            ));
        }
    }
}

/// Walk already-completed dot segments (`rest_after_first_dot`, e.g. `package.ver` or `pac` or ``)
/// through a live structured [`Value`] (`$_`, `$lock`, `$env`, …) and emit typed field/column/index
/// completions with live value previews. Selecting a nested container (`Record`, `Table`, `List`,
/// `Output`) inserts a trailing `.` so pressing `Tab` immediately drills into the next level!
fn collect_value_path_completions(
    root: &Value<'_>,
    base_prefix: &str,
    rest_after_first_dot: &str,
    scored: &mut Vec<(i32, CompletionItem)>,
) {
    let parts: Vec<&str> = rest_after_first_dot.split('.').collect();
    let (done_segs, leaf_prefix) = match parts.split_last() {
        Some((last, init)) => (init, *last),
        None => (&[][..], ""),
    };

    let mut cur = root.share();
    let mut built_prefix = String::from(base_prefix);
    for &seg in done_segs {
        if seg.is_empty() {
            return;
        }
        let clean_seg = seg.trim_end_matches('?');
        let next = match &cur {
            Value::Record(r) => r.get(clean_seg).map(Share::share),
            Value::Table(t) => {
                if let Ok(idx) = clean_seg.parse::<usize>() {
                    t.row_record(idx).map(Value::Record)
                } else {
                    t.col_index(clean_seg)
                        .and_then(|ci| t.cols.get(ci))
                        .map(|c| Value::List(c.share()))
                }
            }
            Value::List(items) => {
                if let Ok(idx) = clean_seg.parse::<usize>() {
                    items.get(idx).map(Share::share)
                } else {
                    // Record-list projection: inspect first matching record for nested completion
                    items.iter().find_map(|it| match it {
                        Value::Record(r) => r.get(clean_seg).map(Share::share),
                        _ => None,
                    })
                }
            }
            Value::Output(o) => match clean_seg {
                "stdout" => Some(o.stdout_value(Alloc::global())),
                "stderr" => Some(o.stderr_value(Alloc::global())),
                "exit_code" | "status" => Some(Value::Int(i64::from(o.exit_code))),
                "duration" => Some(Value::Duration(o.duration)),
                "events" | "chunks" | "streams" => Some(Value::Table(o.events_table(Alloc::global()))),
                _ => None,
            },
            _ => None,
        };
        let Some(nval) = next else {
            return;
        };
        built_prefix.push('.');
        built_prefix.push_str(seg);
        cur = nval;
    }

    let preview_val = |v: &Value<'_>| -> (String, String, bool) {
        let is_container = matches!(
            v,
            Value::Record(_) | Value::Table(_) | Value::List(_) | Value::Output(_)
        );
        match v {
            Value::Record(r) => {
                let mut ks = String::new();
                for (i, (k, _)) in r.iter().take(4).enumerate() {
                    if i > 0 {
                        ks.push_str(", ");
                    }
                    ks.push_str(k);
                }
                if r.len() > 4 {
                    ks.push_str(&format!(", +{}", r.len() - 4));
                }
                (format!("record{{{}}}", r.len()), ks, is_container)
            }
            Value::Table(t) => (
                format!("table[{}]", t.nrows()),
                format!("{} rows × {} cols", t.nrows(), t.ncols()),
                is_container,
            ),
            Value::List(items) => (
                format!("list[{}]", items.len()),
                format!("{} items", items.len()),
                is_container,
            ),
            Value::Output(o) => (
                "output".to_owned(),
                format!("exit {} · {} B stdout", o.exit_code, o.stdout_len()),
                is_container,
            ),
            other => {
                let mut s = blaze_own::AString::new_in(Alloc::global());
                let _ = other.format_cell(&mut s);
                let mut preview = s.as_str().to_owned();
                if preview.chars().count() > 52 {
                    let cut = preview.char_indices().nth(51).map(|(i, _)| i).unwrap_or(preview.len());
                    preview.truncate(cut);
                    preview.push('…');
                }
                (other.type_name().to_owned(), preview, false)
            }
        }
    };

    let push_field = |scored: &mut Vec<(i32, CompletionItem)>,
                      rank_bonus: i32,
                      key: &str,
                      detail: String,
                      doc: String,
                      is_container: bool| {
        if let Some((score, idxs)) = fuzzy_match_score(key, leaf_prefix) {
            let mut insert = String::with_capacity(built_prefix.len() + key.len() + 2);
            insert.push_str(&built_prefix);
            insert.push('.');
            insert.push_str(key);
            if is_container {
                insert.push('.');
            }
            scored.push((
                score + 80 + rank_bonus,
                CompletionItem {
                    label: key.to_owned(),
                    insert,
                    detail,
                    doc,
                    kind: CompletionKind::Column,
                    icon: None,
                    avatar_path: None,
                    fs_meta: None,
                    match_indices: idxs,
                },
            ));
        }
    };

    match &cur {
        Value::Record(r) => {
            let total = r.len();
            for (idx, (k, v)) in r.iter().enumerate() {
                let (detail, doc, is_cont) = preview_val(v);
                let rank = (total.saturating_sub(idx)).min(24) as i32;
                push_field(scored, rank, k, detail, doc, is_cont);
            }
        }
        Value::Table(t) => {
            let ncols = t.ncols();
            for ci in 0..ncols {
                let cname = t.columns.get(ci).unwrap().as_ref();
                let sample = t
                    .cell(0, ci)
                    .map(|v| {
                        let (_, doc, _) = preview_val(v);
                        doc
                    })
                    .unwrap_or_default();
                let doc = if sample.is_empty() {
                    format!("column · {} rows", t.nrows())
                } else {
                    format!("column ({} rows) · e.g. {sample}", t.nrows())
                };
                let rank = (ncols.saturating_sub(ci)).min(24) as i32 + 4;
                push_field(scored, rank, cname, "col".to_owned(), doc, false);
            }
            for ri in 0..t.nrows().min(4) {
                let idx_s = ri.to_string();
                let doc = format!("row {ri} of {}", t.nrows());
                push_field(scored, 0, &idx_s, "row".to_owned(), doc, true);
            }
        }
        Value::List(items) => {
            // If list of records, suggest record keys first!
            if let Some(Value::Record(first_rec)) = items.iter().find(|it| matches!(it, Value::Record(_))) {
                let total = first_rec.len();
                for (idx, (k, v)) in first_rec.iter().enumerate() {
                    let (detail, doc, _) = preview_val(v);
                    let rank = (total.saturating_sub(idx)).min(24) as i32 + 6;
                    push_field(scored, rank, k, detail, doc, false);
                }
            }
            for i in 0..items.len().min(6) {
                if let Some(v) = items.get(i) {
                    let (detail, doc, is_cont) = preview_val(v);
                    let idx_s = i.to_string();
                    push_field(scored, 0, &idx_s, detail, doc, is_cont);
                }
            }
        }
        Value::Output(o) => {
            for (idx, (name, detail, doc)) in [
                ("stdout", "string", format!("{} bytes stdout", o.stdout_len())),
                ("stderr", "string", format!("{} bytes stderr", o.stderr_len())),
                ("exit_code", "int", format!("= {}", o.exit_code)),
                ("duration", "duration", "command runtime".to_owned()),
                ("events", "table", "time-interleaved stream chunks".to_owned()),
            ]
            .into_iter()
            .enumerate()
            {
                push_field(scored, (10 - idx as i32).max(0), name, detail.to_owned(), doc, false);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kill_to_line_start_single_line_and_undo() {
        let mut ed = Editor::new();
        ed.set_text_initial("ls -la /tmp".to_owned());
        ed.kill_to_line_start();
        assert_eq!(ed.text, "");
        assert_eq!(ed.cursor, 0);
        assert_eq!(ed.selection_range(), None);

        // Undo restores original text and cursor.
        assert!(ed.undo());
        assert_eq!(ed.text, "ls -la /tmp");
        assert_eq!(ed.cursor, 11);
    }

    #[test]
    fn kill_to_line_start_middle_of_line() {
        let mut ed = Editor::new();
        ed.set_text_initial("echo hello world".to_owned());
        ed.cursor = 11; // right after "echo hello "
        ed.kill_to_line_start();
        assert_eq!(ed.text, "world");
        assert_eq!(ed.cursor, 0);

        // Calling again at cursor 0 is a no-op and does not pollute undo.
        ed.kill_to_line_start();
        assert_eq!(ed.text, "world");
        assert_eq!(ed.cursor, 0);
        assert!(ed.undo());
        assert_eq!(ed.text, "echo hello world");
        assert_eq!(ed.cursor, 11);
    }

    #[test]
    fn kill_to_line_start_multiline_line_two() {
        let mut ed = Editor::new();
        ed.set_text_initial("let x = 1\necho hello world".to_owned());
        ed.cursor = "let x = 1\necho hello".len();
        ed.kill_to_line_start();
        assert_eq!(ed.text, "let x = 1\n world");
        assert_eq!(ed.cursor, "let x = 1\n".len());
    }

    #[test]
    fn kill_to_line_start_empty_continuation_line_joins_above() {
        let mut ed = Editor::new();
        ed.set_text_initial("let x = 1\n".to_owned());
        assert_eq!(ed.cursor, 10);
        ed.kill_to_line_start();
        assert_eq!(ed.text, "let x = 1");
        assert_eq!(ed.cursor, 9);

        // Also works at column 0 of a non-empty continuation line.
        ed.set_text_initial("line1\nline2".to_owned());
        ed.cursor = 6;
        ed.kill_to_line_start();
        assert_eq!(ed.text, "line1line2");
        assert_eq!(ed.cursor, 5);
        assert!(ed.undo());
        assert_eq!(ed.text, "line1\nline2");
        assert_eq!(ed.cursor, 6);
    }

    #[test]
    fn kill_to_line_start_deletes_active_selection() {
        let mut ed = Editor::new();
        ed.set_text_initial("hello cruel world".to_owned());
        ed.selection_anchor = Some(6);
        ed.cursor = 12;
        ed.kill_to_line_start();
        assert_eq!(ed.text, "hello world");
        assert_eq!(ed.cursor, 6);
        assert_eq!(ed.selection_range(), None);

        assert!(ed.undo());
        assert_eq!(ed.text, "hello cruel world");
    }

    #[test]
    fn tab_completion_auto_completes_common_dir_prefix_and_populates_fs_meta() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("blaze-tab-lcp-{}-{nanos}", std::process::id()));
        let vol = root.join("Volumes");
        let proj = vol.join("Projects");
        let blaze_a = proj.join("blaze");
        let blaze_b = proj.join("blaze-shell-1124");
        let sample_file = proj.join("notes.txt");
        fs::create_dir_all(&blaze_a).unwrap();
        fs::create_dir_all(&blaze_b).unwrap();
        fs::write(&sample_file, "hello world").unwrap();

        let state = EngineState::new(&root.to_string_lossy(), &[], blaze_own::Alloc::global());
        let mut ed = Editor::new();
        // Seed zoxide with multiple descendants under `<root>/Volumes/Projects/...` + an unrelated path.
        let now = nanos as i64;
        ed.zoxide.record_visit(&ed.local_host, &proj.to_string_lossy(), now);
        ed.zoxide.record_visit(&ed.local_host, &blaze_a.to_string_lossy(), now);
        ed.zoxide.record_visit(&ed.local_host, &blaze_b.to_string_lossy(), now);
        ed.zoxide.record_visit(&ed.local_host, "/Users/other/voice", now);

        // 1. `cd <root>/Vo<Tab>` immediately auto-completes `<root>/Volumes/` and dismisses popup!
        ed.set_text_initial(format!("cd {}/Vo", root.to_string_lossy()));
        ed.trigger_completion(&state);
        assert_eq!(ed.text, format!("cd {}/", vol.to_string_lossy()));
        assert!(ed.completions.is_empty(), "exact dir LCP should dismiss popup so user can keep typing");

        // 2. Keep typing `Proj` + `<Tab>` -> immediately completes `<root>/Volumes/Projects/`!
        ed.insert_typed("Proj");
        ed.trigger_completion(&state);
        assert_eq!(ed.text, format!("cd {}/", proj.to_string_lossy()));
        assert!(ed.completions.is_empty());

        // 3. Typing ambiguous `bl` + `<Tab>` extends to `blaze` and opens narrowed 2-item popup!
        ed.insert_typed("bl");
        ed.trigger_completion(&state);
        assert_eq!(ed.text, format!("cd {}/blaze", proj.to_string_lossy()));
        assert_eq!(ed.completions.len(), 2);
        assert!(ed.completions.iter().all(|c| c.fs_meta.is_some()));

        // 4. File completions carry mtime_ns and exact size_bytes for heatmap + size bars.
        let mut file_hits = Vec::new();
        collect_path_completions(&proj.to_string_lossy(), "not", false, &mut file_hits);
        let note = file_hits.iter().find(|(_, it)| it.label == "notes.txt").expect("notes.txt found");
        let meta = note.1.fs_meta.as_ref().expect("fs_meta populated");
        assert_eq!(meta.size_bytes, Some(11));
        assert!(meta.mtime_ns.is_some());

        // 5. Unique direct local folder fuzzy match (`cd rg<Tab>` in `big-repos/` with `ripgrep/`,
        // `ruff/`, `tokio/`) must beat distant zoxide `/Volumes/Projects/andrewgazelka/rgb` and
        // expand immediately on first Tab!
        let big = root.join("big-repos");
        fs::create_dir_all(big.join("ripgrep")).unwrap();
        fs::create_dir_all(big.join("ruff")).unwrap();
        fs::create_dir_all(big.join("tokio")).unwrap();
        let big_state = EngineState::new(&big.to_string_lossy(), &[], blaze_own::Alloc::global());
        ed.zoxide.record_visit(&ed.local_host, "/Volumes/Projects/andrewgazelka/rgb", now);
        ed.set_text_initial("cd rg".to_owned());
        ed.trigger_completion(&big_state);
        assert_eq!(ed.text, "cd ripgrep/");
        assert!(ed.completions.is_empty());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unified_zero_tab_completions_select_most_likely_and_sync_ghost() {
        let state = EngineState::new("/tmp", &[], blaze_own::Alloc::global());
        let mut ed = Editor::new();

        // 1. `git ` -> unified completion popup auto-opens (zero Tab!) with `status` selected #1.
        ed.set_text_initial("git ".to_owned());
        ed.refresh_assist(&state, &[], false);
        assert!(ed.completions.len() >= 10);
        assert_eq!(ed.completion_selected, 0);
        assert_eq!(ed.completions[0].label, "status");
        assert!(ed.completions[0].doc.contains("working tree"));

        // 2. `git s` -> `status` is #1 (matching ghost `tatus`) and pressing Tab completes `git status `!
        ed.set_text_initial("git s".to_owned());
        ed.refresh_assist(&state, &[], false);
        assert_eq!(ed.completions[0].label, "status");
        assert_eq!(ed.ghost.as_deref(), Some("tatus"));

        // 3. `git st` -> `status` STILL beats `stash` (no ghost/popup mismatch!) and Tab accepts it!
        ed.set_text_initial("git st".to_owned());
        ed.refresh_assist(&state, &[], false);
        assert_eq!(ed.completions[0].label, "status");
        assert_eq!(ed.ghost.as_deref(), Some("atus"));
        assert!(ed.completions.iter().any(|i| i.label == "stash"));

        // Arrowing down to `stash` live-syncs the inline ghost text to `ash`!
        ed.move_down();
        assert_eq!(ed.completions[ed.completion_selected].label, "stash");
        assert_eq!(ed.ghost.as_deref(), Some("ash"));
        // Arrowing back up restores `status` + ghost `atus`, and Tab applies `git status `!
        ed.move_up();
        assert_eq!(ed.completions[ed.completion_selected].label, "status");
        assert!(ed.apply_selected_completion());
        assert_eq!(ed.text, "git status ");

        // 4. `git add` -> exact unique subcommand dismisses candidate popup and shows 1-line Signature + flags!
        ed.set_text_initial("git add".to_owned());
        ed.refresh_assist(&state, &[], false);
        assert!(ed.completions.is_empty());
        match ed.live_inline_hint().expect("git add signature") {
            LiveHint::Signature { signature, about, flags, .. } => {
                assert!(signature.starts_with("git add"));
                assert!(about.contains("Stage"));
                assert!(flags.iter().any(|(f, _)| f == "-A"));
            }
        }

        // 5. `git add -` -> unified popup auto-opens with curated flag candidates + descriptions.
        ed.set_text_initial("git add -".to_owned());
        ed.refresh_assist(&state, &[], false);
        assert!(!ed.completions.is_empty());
        assert!(ed.completions.iter().all(|i| i.kind == CompletionKind::Flag));
        assert!(ed.completions.iter().any(|i| i.label == "-A"));

        // 6. `bgit worktree ` -> nested subcommands straight from `bgit::cli_spec()`!
        ed.set_text_initial("bgit worktree ".to_owned());
        ed.refresh_assist(&state, &[], false);
        assert!(ed.completions.iter().any(|i| i.label == "add"));
        assert!(ed.completions.iter().any(|i| i.label == "sync"));

        // 7. `bgit wt add ` -> exact nested signature with positionals & flags!
        ed.set_text_initial("bgit wt add ".to_owned());
        ed.refresh_assist(&state, &[], false);
        assert!(ed.completions.is_empty());
        match ed.live_inline_hint().expect("bgit wt add signature") {
            LiveHint::Signature { signature, about, flags, .. } => {
                assert!(signature.contains("add"));
                assert!(about.contains("worktree"));
                assert!(flags.iter().any(|(f, _)| f == "--vfs"));
            }
        }

        // 8. Esc suppresses auto-popup on the current token until moving on.
        ed.set_text_initial("git s".to_owned());
        ed.refresh_assist(&state, &[], false);
        assert!(!ed.completions.is_empty());
        ed.suppress_completions();
        assert!(ed.completions.is_empty());
        ed.refresh_assist(&state, &[], false);
        assert!(ed.completions.is_empty(), "Esc suppression must keep popup closed on same token");
    }

    #[test]
    fn dot_subject_and_var_path_completions_drill_into_previous_output() {
        let alloc = Alloc::global();
        let mut state = EngineState::new("/tmp", &[], alloc);
        let mut pkg = blaze_sh::Record::new(alloc);
        pkg.insert(blaze_own::Rc::from_str_in("name", alloc), Value::string("hello-world", alloc));
        pkg.insert(blaze_own::Rc::from_str_in("version", alloc), Value::string("0.1.0", alloc));
        pkg.insert(blaze_own::Rc::from_str_in("edition", alloc), Value::string("blaze-2026.11", alloc));
        let mut root = blaze_sh::Record::new(alloc);
        root.insert(blaze_own::Rc::from_str_in("package", alloc), Value::Record(pkg));
        state.set_var("_", Value::Record(root.clone()), false);
        state.set_var("lock", Value::Record(root), false);

        let mut ed = Editor::new();

        // 1. Typing `.pac` auto-opens with `package` selected #1 and drills into `.package.` on Tab!
        ed.set_text_initial(".pac".to_owned());
        ed.refresh_assist(&state, &[], false);
        assert_eq!(ed.completions.len(), 1);
        assert_eq!(ed.completions[0].label, "package");
        assert_eq!(ed.completions[0].insert, ".package.");
        assert!(ed.apply_selected_completion());
        assert_eq!(ed.text, ".package.");

        // 2. Refreshing after `.package.` immediately shows `name`, `version`, `edition` with live previews!
        ed.refresh_assist(&state, &[], false);
        assert_eq!(ed.completions.len(), 3);
        let ver = ed.completions.iter().find(|c| c.label == "version").expect("version field");
        assert_eq!(ver.insert, ".package.version");
        assert_eq!(ver.doc, "0.1.0");

        // 3. Typing `$lock.package.v` completes `$lock.package.version` without trailing space.
        ed.set_text_initial("$lock.package.v".to_owned());
        ed.refresh_assist(&state, &[], false);
        assert_eq!(ed.completions[0].label, "version");
        assert!(ed.apply_selected_completion());
        assert_eq!(ed.text, "$lock.package.version");

        // 4. Piped `open Cargo.lock | .pack` parses the file on-the-fly (cached by mtime)
        // and auto-completes `.package.` -> `name`, `version`, `source`!
        let tmp_dir = std::env::temp_dir().join(format!("blaze-pipe-dot-{}", std::process::id()));
        let _ = fs::create_dir_all(&tmp_dir);
        let lock_path = tmp_dir.join("Cargo.lock");
        fs::write(
            &lock_path,
            "version = 4\n\n[[package]]\nname = \"adler2\"\nversion = \"2.0.1\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
        )
        .unwrap();
        let tmp_state = EngineState::new(&tmp_dir.to_string_lossy(), &[], alloc);
        ed.set_text_initial("open Cargo.lock | .pack".to_owned());
        ed.refresh_assist(&tmp_state, &[], false);
        assert_eq!(ed.completions.len(), 1);
        assert_eq!(ed.completions[0].label, "package");
        assert_eq!(ed.completions[0].insert, ".package.");
        assert!(ed.apply_selected_completion());
        assert_eq!(ed.text, "open Cargo.lock | .package.");

        ed.refresh_assist(&tmp_state, &[], false);
        assert!(ed.completions.iter().any(|c| c.label == "name"));
        assert!(ed.completions.iter().any(|c| c.label == "version"));
        assert!(ed.completions.iter().any(|c| c.label == "source"));

        // 5. Also supports `$lock | .pack`, `open Cargo.lock | .package | .ver`,
        // bare structured file `Cargo.lock.pac`, and leading `| .pac`!
        ed.set_text_initial("open Cargo.lock | .package | .ver".to_owned());
        ed.refresh_assist(&tmp_state, &[], false);
        assert_eq!(ed.completions[0].label, "version");
        assert_eq!(ed.completions[0].insert, ".version");

        ed.set_text_initial("Cargo.lock.pac".to_owned());
        ed.refresh_assist(&tmp_state, &[], false);
        assert_eq!(ed.completions[0].label, "package");
        assert_eq!(ed.completions[0].insert, "Cargo.lock.package.");
        assert!(ed.apply_selected_completion());
        assert_eq!(ed.text, "Cargo.lock.package.");

        ed.refresh_assist(&tmp_state, &[], false);
        assert!(ed.completions.iter().any(|c| c.label == "name"));
        assert!(ed.completions.iter().any(|c| c.label == "version"));

        ed.set_text_initial("| .pac".to_owned());
        ed.refresh_assist(&state, &[], false);
        assert_eq!(ed.completions[0].label, "package");
        assert_eq!(ed.completions[0].insert, ".package.");

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn default_auto_open_ranks_data_files_exact_case_and_never_flashes_red() {
        let alloc = Alloc::global();
        let tmp_dir = std::env::temp_dir().join(format!("blaze-ac-overhaul-{}", std::process::id()));
        let _ = fs::create_dir_all(&tmp_dir);
        fs::write(
            tmp_dir.join("Cargo.lock"),
            "version = 4\n\n[[package]]\nname = \"adler2\"\nversion = \"2.0.1\"\n",
        )
        .unwrap();
        fs::write(
            tmp_dir.join("Cargo.toml"),
            "[package]\nname = \"hello\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let tmp_cwd = tmp_dir.to_string_lossy().into_owned();
        let state = EngineState::new(&tmp_cwd, &[], alloc);
        let mut ed = Editor::new();

        // 1. Typing `Car` auto-opens BY DEFAULT (open_popup = false!) with `Cargo.lock` & `Cargo.toml`
        //    ranked above lowercase `$PATH` binaries, exact-case ghost, and NEVER red!
        ed.set_text_initial("Car".to_owned());
        ed.refresh_assist(&state, &[], false);
        assert!(!ed.completions.is_empty(), "Car must auto-open by default without Tab");
        assert!(
            matches!(ed.completions[0].label.as_str(), "Cargo.lock" | "Cargo.toml"),
            "expected Cargo.lock or Cargo.toml at #1 for `Car`, got {:?}",
            ed.completions.iter().take(5).map(|c| &c.label).collect::<Vec<_>>()
        );
        assert!(
            ed.ghost.as_deref().is_some_and(|g| g == "go.lock" || g == "go.toml"),
            "ghost must match exact case (never `go-rm`), got {:?}",
            ed.ghost
        );
        assert!(
            is_known_command_prefix("Car", Some(&tmp_cwd), None),
            "`Car` must not flash red while typing"
        );

        // 2. Lexer highlights bare `Cargo.lock.package` as String + Punct + Var (never Command/red!).
        let spans = blaze_sh::lex::highlight("Cargo.lock.package", alloc);
        assert!(
            spans.iter().all(|s| s.kind != blaze_sh::lex::HighlightKind::Command),
            "bare data file must never highlight as Command, got {:?}",
            spans.as_slice()
        );
        assert_eq!(spans[0].kind, blaze_sh::lex::HighlightKind::String);

        // 3. `Cargo.lock.package | sort ` and `sort -` auto-open live table columns!
        ed.set_text_initial("Cargo.lock.package | sort ".to_owned());
        ed.refresh_assist(&state, &[], false);
        assert!(ed.completions.iter().any(|c| c.label == "name" && c.kind == CompletionKind::Column));
        assert!(ed.completions.iter().any(|c| c.label == "version" && c.kind == CompletionKind::Column));

        ed.set_text_initial("Cargo.lock.package | sort -".to_owned());
        ed.refresh_assist(&state, &[], false);
        assert!(ed.completions.iter().any(|c| c.label == "-name" && c.kind == CompletionKind::Column));

        // 4. Inside quotes or comments, popup stays quiet by default.
        ed.set_text_initial("echo \"hello".to_owned());
        ed.refresh_assist(&state, &[], false);
        assert!(ed.completions.is_empty(), "must not auto-open inside string literal");

        let _ = fs::remove_dir_all(&tmp_dir);
    }
}
