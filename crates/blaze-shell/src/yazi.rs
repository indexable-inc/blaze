//! Native Yazi-class 3-column GPU file manager (`y` / `y <dir>`) powered by our sans-IO
//! `blaze-sniff` + `blaze-preview` crates:
//! - Miller columns (parent | current | rich structured preview) in Metal
//! - Tabular previews for `uv.lock`, `Cargo.lock`, `blaze.lock`, CSV, JSON arrays
//! - Structured key/value + deps tables for `Cargo.toml`, `package.json`, `flake.lock`, TOML
//! - Rendered Markdown (`README.md`, `AGENTS.md`, `*.md`) with headings, bullets, inline code
//!   pills, blockquotes, and syntax-highlighted code fences
//! - Syntax-highlighted source code for Rust, TS/JS, Python, Go, Nix, Shell, SQL, etc.
//! - Vim navigation (`j`/`k`/`h`/`l`/`gg`/`G`/`/` filter/`Enter`/`Space` insert/`q`/`Esc`).

use std::fs;
use std::path::{Path, PathBuf};

use blaze_own::Alloc;
use blaze_preview::{
    CodeLine, MdBlock, MdSpan, MdStyle, MdTable, PreviewBody, PreviewDoc, PreviewKv,
    PreviewLimits, PreviewTable, TextAlign, TokenKind, build_preview,
};

use crate::blocks::BlockStore;
use crate::layout::{TOP_BAR_H, layout_top_bar};
use crate::prompt::PromptInfo;
use crate::svg_icons::icon_name_for_file;
use crate::theme;
use crate::view::{
    CellMetrics, DirPreviewEntry, HoverKind, PreviewLine, PreviewSpan, Scene, TextFace,
    truncate_chars,
};

const MAX_PREVIEW_BYTES: usize = 256 * 1024;
const MAX_DIR_ENTRIES: usize = 512;

const YAZI_PREVIEW_LIMITS: PreviewLimits = PreviewLimits {
    max_rows: 512,
    max_cols: 6,
    max_lines: 400,
    max_cell_chars: 64,
};

fn format_size_bytes(bytes: i64) -> String {
    let mut s = String::new();
    let _ = blaze_sh::Value::Size(blaze_sh::Bytes(bytes)).format_cell(&mut s);
    s
}

/// One entry inside a Miller column.
#[derive(Clone, Debug)]
pub struct YaziEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub size: u64,
    pub icon: &'static str,
}

/// Rich preview content for the currently highlighted entry in the right pane.
#[derive(Clone, Debug)]
pub enum YaziPreview {
    Empty,
    Directory { meta: String, items: Vec<YaziEntry> },
    Rich(PreviewDoc<'static>),
    Error(String),
}

/// Outcome returned by [`YaziState::handle_key`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum YaziAction {
    Continue,
    ExitAndCd(PathBuf),
    ExitCancel,
    InsertPath(String),
}

/// Live state for the built-in `y` (Yazi-class) 3-column file manager.
pub struct YaziState {
    pub cwd: PathBuf,
    pub parent_entries: Vec<YaziEntry>,
    pub parent_selected: usize,
    pub entries: Vec<YaziEntry>,
    pub selected: usize,
    pub scroll: usize,
    pub preview_scroll: usize,
    pub preview_max_scroll: usize,
    pub preview_split_x: f32,
    pub filter: String,
    pub filtering: bool,
    pub show_hidden: bool,
    pub pending_g: bool,
    pub preview: YaziPreview,
}

impl YaziState {
    pub fn open(dir: &Path) -> Self {
        let cwd = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        let mut st = Self {
            cwd,
            parent_entries: Vec::new(),
            parent_selected: 0,
            entries: Vec::new(),
            selected: 0,
            scroll: 0,
            preview_scroll: 0,
            preview_max_scroll: 0,
            preview_split_x: 420.0,
            filter: String::new(),
            filtering: false,
            show_hidden: false,
            pending_g: false,
            preview: YaziPreview::Empty,
        };
        st.reload(None);
        st
    }

    pub fn visible_indices(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.entries.len()).collect();
        }
        let needle = self.filter.to_ascii_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.name.to_ascii_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect()
    }

    pub fn current_entry(&self) -> Option<&YaziEntry> {
        let vis = self.visible_indices();
        let idx = *vis.get(self.selected)?;
        self.entries.get(idx)
    }

    pub fn reload(&mut self, select_name: Option<&str>) {
        self.entries = read_dir_entries(&self.cwd, self.show_hidden);
        if let Some(parent) = self.cwd.parent() {
            self.parent_entries = read_dir_entries(parent, self.show_hidden);
            let cur_name = self.cwd.file_name().and_then(|s| s.to_str()).unwrap_or("");
            self.parent_selected = self
                .parent_entries
                .iter()
                .position(|e| e.name == cur_name)
                .unwrap_or(0);
        } else {
            self.parent_entries.clear();
            self.parent_selected = 0;
        }
        self.selected = select_name
            .and_then(|n| self.entries.iter().position(|e| e.name == n))
            .unwrap_or(0);
        self.scroll = 0;
        self.refresh_preview();
    }

    pub fn refresh_preview(&mut self) {
        self.preview_scroll = 0;
        self.preview_max_scroll = 0;
        let Some(entry) = self.current_entry().cloned() else {
            self.preview = YaziPreview::Empty;
            return;
        };
        if entry.is_dir {
            let (sub, _, markers) = read_dir_entries_detailed(&entry.path, self.show_hidden);
            let meta = format_dir_summary(&entry.name, &sub, markers);
            self.preview = YaziPreview::Directory { meta, items: sub };
            return;
        }
        self.preview = load_rich_preview(&entry.path, &entry.name, entry.size);
    }

    /// Handle macOS trackpad / mouse-wheel scrolling in either the file column or the RHS preview pane!
    pub fn scroll_at(&mut self, x: f32, delta_rows: isize) -> bool {
        if delta_rows == 0 {
            return false;
        }
        if x >= self.preview_split_x {
            let next = if delta_rows > 0 {
                self.preview_scroll.saturating_add(delta_rows as usize)
            } else {
                self.preview_scroll.saturating_sub(delta_rows.unsigned_abs())
            }
            .min(self.preview_max_scroll);
            let changed = next != self.preview_scroll;
            self.preview_scroll = next;
            changed
        } else {
            let vis_len = self.visible_indices().len();
            if vis_len == 0 {
                return false;
            }
            let next = if delta_rows > 0 {
                self.selected.saturating_add(delta_rows as usize).min(vis_len - 1)
            } else {
                self.selected.saturating_sub(delta_rows.unsigned_abs())
            };
            if next != self.selected {
                self.selected = next;
                self.refresh_preview();
                true
            } else {
                false
            }
        }
    }

    pub fn handle_key(&mut self, key: &str, ch: Option<char>) -> YaziAction {
        if self.filtering {
            match key {
                "Escape" => {
                    self.filtering = false;
                    self.filter.clear();
                    self.selected = 0;
                    self.refresh_preview();
                }
                "Enter" => {
                    self.filtering = false;
                    self.refresh_preview();
                }
                "Backspace" => {
                    self.filter.pop();
                    self.selected = 0;
                    self.refresh_preview();
                }
                _ => {
                    if let Some(c) = ch
                        && !c.is_control()
                    {
                        self.filter.push(c);
                        self.selected = 0;
                        self.refresh_preview();
                    }
                }
            }
            return YaziAction::Continue;
        }

        let vis_len = self.visible_indices().len();
        if self.pending_g {
            self.pending_g = false;
            if ch == Some('g') {
                self.selected = 0;
                self.refresh_preview();
                return YaziAction::Continue;
            }
        }

        match key {
            "Escape" => return YaziAction::ExitAndCd(self.cwd.clone()),
            "Up" => {
                if self.selected > 0 {
                    self.selected -= 1;
                    self.refresh_preview();
                }
                return YaziAction::Continue;
            }
            "Down" => {
                if self.selected + 1 < vis_len {
                    self.selected += 1;
                    self.refresh_preview();
                }
                return YaziAction::Continue;
            }
            "Left" => {
                self.go_parent();
                return YaziAction::Continue;
            }
            "Right" | "Enter" => {
                if let Some(entry) = self.current_entry().cloned() {
                    if entry.is_dir {
                        self.cwd = entry.path;
                        self.filter.clear();
                        self.reload(None);
                    } else if key == "Enter" {
                        return YaziAction::InsertPath(entry.path.to_string_lossy().into_owned());
                    }
                }
                return YaziAction::Continue;
            }
            _ => {}
        }

        match ch {
            Some('q') => YaziAction::ExitAndCd(self.cwd.clone()),
            Some('Q') => YaziAction::ExitCancel,
            Some('j') => {
                if self.selected + 1 < vis_len {
                    self.selected += 1;
                    self.refresh_preview();
                }
                YaziAction::Continue
            }
            Some('k') => {
                if self.selected > 0 {
                    self.selected -= 1;
                    self.refresh_preview();
                }
                YaziAction::Continue
            }
            Some('J') => {
                self.preview_scroll = (self.preview_scroll + 4).min(self.preview_max_scroll);
                YaziAction::Continue
            }
            Some('K') => {
                self.preview_scroll = self.preview_scroll.saturating_sub(4);
                YaziAction::Continue
            }
            Some('h') => {
                self.go_parent();
                YaziAction::Continue
            }
            Some('l') => {
                if let Some(entry) = self.current_entry().cloned()
                    && entry.is_dir
                {
                    self.cwd = entry.path;
                    self.filter.clear();
                    self.reload(None);
                }
                YaziAction::Continue
            }
            Some('g') => {
                self.pending_g = true;
                YaziAction::Continue
            }
            Some('G') => {
                if vis_len > 0 {
                    self.selected = vis_len - 1;
                    self.refresh_preview();
                }
                YaziAction::Continue
            }
            Some('.') => {
                self.show_hidden = !self.show_hidden;
                let cur = self.current_entry().map(|e| e.name.clone());
                self.reload(cur.as_deref());
                YaziAction::Continue
            }
            Some('/') => {
                self.filtering = true;
                self.filter.clear();
                self.selected = 0;
                self.refresh_preview();
                YaziAction::Continue
            }
            Some(' ') | Some('i') => {
                if let Some(entry) = self.current_entry() {
                    YaziAction::InsertPath(entry.path.to_string_lossy().into_owned())
                } else {
                    YaziAction::Continue
                }
            }
            _ => YaziAction::Continue,
        }
    }

    fn go_parent(&mut self) {
        let prev_name = self.cwd.file_name().and_then(|s| s.to_str()).map(str::to_owned);
        if let Some(parent) = self.cwd.parent().map(Path::to_path_buf) {
            self.cwd = parent;
            self.filter.clear();
            self.reload(prev_name.as_deref());
        }
    }
}

fn load_rich_preview(path: &Path, name: &str, size: u64) -> YaziPreview {
    use std::io::Read as _;
    let Ok(mut f) = fs::File::open(path) else {
        return YaziPreview::Error("permission denied".to_owned());
    };
    let mut buf = vec![0u8; (size as usize).min(MAX_PREVIEW_BYTES)];
    let n = f.read(&mut buf).unwrap_or(0);
    buf.truncate(n);
    let doc = build_preview(name, &buf, size, YAZI_PREVIEW_LIMITS, Alloc::global());
    YaziPreview::Rich(doc)
}

/// Hydrate a hovered `ls` / `git status` file or folder entry into a rich `HoverKind::DirPreview`
/// (with SVG icons, `blaze-sniff` folder analysis, semantic tags, and right-aligned sizes) or
/// `HoverKind::FilePreview` (syntax highlighting, rendered Markdown, tabular lockfiles).
pub fn load_hover_preview(
    path: &Path,
    name: &str,
    icon: Option<&'static str>,
    max_lines: usize,
) -> HoverKind {
    if let Ok(meta) = fs::metadata(path)
        && meta.is_dir()
    {
        let (all_entries, hidden_count, markers) = read_dir_entries_detailed(path, false);
        let total_items = all_entries.len();
        let summary = format_dir_summary(name, &all_entries, markers);
        let entries: Vec<DirPreviewEntry> = all_entries
            .into_iter()
            .take(max_lines)
            .map(|e| {
                let (kind_label, muted) = entry_kind_label(&e);
                let size_label = if e.is_dir {
                    String::new()
                } else {
                    format_size_bytes(e.size as i64)
                };
                DirPreviewEntry {
                    name: e.name,
                    icon: e.icon,
                    is_dir: e.is_dir,
                    size_label,
                    kind_label,
                    muted,
                }
            })
            .collect();
        return HoverKind::DirPreview {
            full_path: path.to_string_lossy().into_owned(),
            name: name.to_owned(),
            icon: icon.or_else(|| Some(icon_name_for_file(name, Some("dir")))),
            meta: summary,
            entries,
            total_items,
            hidden_count,
        };
    }
    let (meta, lines, truncated) = load_file_preview_lines(path, max_lines);
    HoverKind::FilePreview {
        full_path: path.to_string_lossy().into_owned(),
        name: name.to_owned(),
        icon,
        meta,
        lines,
        truncated,
    }
}

/// Read a bounded, rich styled file preview for `ls`/`git status` hover popovers (powered by
/// `blaze-sniff` + `blaze-preview`: full syntax highlighting, rendered Markdown, & tabular lockfiles!).
pub fn load_file_preview_lines(path: &Path, max_lines: usize) -> (String, Vec<PreviewLine>, bool) {
    let plain = |s: String, col: [f32; 4], italic: bool| PreviewLine {
        line_no: None,
        spans: vec![PreviewSpan { text: s, color: col, bold: false, italic }],
    };
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) => return ("unreadable".to_owned(), vec![plain(format!("{e}"), theme::DANGER, false)], false),
    };
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if meta.is_dir() {
        let entries = read_dir_entries(path, false);
        let total = entries.len();
        let lines: Vec<PreviewLine> = entries
            .into_iter()
            .take(max_lines)
            .map(|e| {
                let lbl = if e.is_dir { format!("{}/", e.name) } else { e.name };
                let col = if e.is_dir { theme::INK_1 } else { theme::INK_2 };
                plain(lbl, col, false)
            })
            .collect();
        let meta_str = format!("dir · {total} items");
        return (meta_str, lines, total > max_lines);
    }
    let size = meta.len();
    match load_rich_preview(path, name, size) {
        YaziPreview::Rich(doc) => {
            let mut kind_lbl = String::new();
            let _ = doc.sniff.format_label(&mut kind_lbl);
            let meta_str = format!("{kind_lbl} · {}", format_size_bytes(size as i64));
            match doc.body {
                PreviewBody::Table(t) => {
                    let mut lines = Vec::new();
                    let hdr = t.headers.iter().map(|h| format!("{:<16}", h.as_str())).collect::<String>();
                    lines.push(PreviewLine {
                        line_no: None,
                        spans: vec![PreviewSpan { text: hdr, color: theme::INK_3, bold: true, italic: false }],
                    });
                    for row in t.rows.iter().take(max_lines.saturating_sub(1)) {
                        let mut spans = Vec::new();
                        for (ci, cell) in row.iter().enumerate() {
                            let col = if ci == 0 {
                                theme::INK_1
                            } else if ci == 1 {
                                theme::rgb(0x7c, 0xc9, 0x8b)
                            } else {
                                theme::INK_3
                            };
                            spans.push(PreviewSpan {
                                text: format!("{:<16}", truncate_chars(cell.as_str(), 15)),
                                color: col,
                                bold: false,
                                italic: false,
                            });
                        }
                        lines.push(PreviewLine { line_no: None, spans });
                    }
                    (meta_str, lines, t.total_rows > max_lines)
                }
                PreviewBody::KeyValues(kv) => {
                    let mut lines = Vec::new();
                    for (k, v) in kv.pairs.iter().take(max_lines) {
                        lines.push(PreviewLine {
                            line_no: None,
                            spans: vec![
                                PreviewSpan {
                                    text: format!("{:<14} ", truncate_chars(k.as_str(), 14)),
                                    color: theme::INK_3,
                                    bold: false,
                                    italic: false,
                                },
                                PreviewSpan {
                                    text: v.as_str().to_owned(),
                                    color: theme::rgb(0x7c, 0xc9, 0x8b),
                                    bold: false,
                                    italic: false,
                                },
                            ],
                        });
                    }
                    if let Some(t) = kv.table {
                        for row in t.rows.iter().take(max_lines.saturating_sub(lines.len())) {
                            let mut spans = Vec::new();
                            for (ci, cell) in row.iter().enumerate() {
                                spans.push(PreviewSpan {
                                    text: format!("{:<14} ", truncate_chars(cell.as_str(), 14)),
                                    color: if ci == 0 { theme::INK_1 } else { theme::INK_3 },
                                    bold: false,
                                    italic: false,
                                });
                            }
                            lines.push(PreviewLine { line_no: None, spans });
                        }
                    }
                    (meta_str, lines, false)
                }
                PreviewBody::Markdown { blocks, truncated } => {
                    let mut lines = Vec::new();
                    let style_to_preview = |style: MdStyle| match style {
                        MdStyle::Bold => (theme::INK_1, true, false),
                        MdStyle::BoldItalic => (theme::INK_1, true, true),
                        MdStyle::Italic => (theme::INK_2, false, true),
                        MdStyle::Code => (theme::rgb(0x7c, 0xc9, 0x8b), false, false),
                        MdStyle::Kbd => (theme::INK_1, true, false),
                        MdStyle::Strike => (theme::INK_4, false, false),
                        MdStyle::Link => (theme::rgb(0x7c, 0xc4, 0xea), false, false),
                        MdStyle::Plain => (theme::INK_2, false, false),
                    };
                    for b in blocks.iter().take(max_lines) {
                        let pl = match b {
                            MdBlock::Heading { spans, .. } => PreviewLine {
                                line_no: None,
                                spans: spans
                                    .iter()
                                    .map(|sp| PreviewSpan {
                                        text: sp.text.as_str().to_owned(),
                                        color: [1.0, 1.0, 1.0, 1.0],
                                        bold: true,
                                        italic: false,
                                    })
                                    .collect(),
                            },
                            MdBlock::Paragraph { spans, .. } | MdBlock::Quote { spans } => PreviewLine {
                                line_no: None,
                                spans: spans
                                    .iter()
                                    .map(|sp| {
                                        let (color, bold, italic) = style_to_preview(sp.style);
                                        PreviewSpan { text: sp.text.as_str().to_owned(), color, bold, italic }
                                    })
                                    .collect(),
                            },
                            MdBlock::Image { alt, src, .. } => {
                                let lbl = if !alt.is_empty() {
                                    format!("[image: {}]", alt.as_str())
                                } else {
                                    format!("[image: {}]", src.as_str())
                                };
                                plain(lbl, theme::INK_3, false)
                            }
                            MdBlock::Bullet { marker, spans, .. } => {
                                let mut out = vec![PreviewSpan {
                                    text: format!("{} ", marker.as_str()),
                                    color: theme::INK_3,
                                    bold: true,
                                    italic: false,
                                }];
                                for sp in spans.iter() {
                                    let (color, bold, italic) = style_to_preview(sp.style);
                                    out.push(PreviewSpan { text: sp.text.as_str().to_owned(), color, bold, italic });
                                }
                                PreviewLine { line_no: None, spans: out }
                            }
                            MdBlock::Table(tbl) => {
                                let summary = format!(
                                    "[table: {} cols × {} rows]",
                                    tbl.headers.len().max(tbl.rows.first().map_or(0, |r| r.len())),
                                    tbl.rows.len()
                                );
                                plain(summary, theme::INK_3, false)
                            }
                            MdBlock::Details { summary, open } => {
                                let mut out = vec![PreviewSpan {
                                    text: if *open { "▾ " } else { "▸ " }.to_owned(),
                                    color: theme::INK_3,
                                    bold: true,
                                    italic: false,
                                }];
                                for sp in summary.iter() {
                                    out.push(PreviewSpan {
                                        text: sp.text.as_str().to_owned(),
                                        color: theme::INK_1,
                                        bold: true,
                                        italic: false,
                                    });
                                }
                                PreviewLine { line_no: None, spans: out }
                            }
                            MdBlock::CodeFence { lang, .. } => plain(format!("[{}]", lang.as_str()), theme::INK_4, true),
                            MdBlock::Rule => plain("────────────────".to_owned(), theme::LINE_STRONG, false),
                            MdBlock::Blank => PreviewLine { line_no: None, spans: Vec::new() },
                        };
                        lines.push(pl);
                    }
                    (meta_str, lines, truncated)
                }
                PreviewBody::Code { lines: cl, truncated, .. } => {
                    let out: Vec<PreviewLine> = cl
                        .iter()
                        .take(max_lines)
                        .map(|l| PreviewLine {
                            line_no: Some(l.line_no),
                            spans: l
                                .spans
                                .iter()
                                .map(|s| {
                                    let (color, bold, italic) = token_color(s.kind);
                                    PreviewSpan {
                                        text: s.text.as_str().replace('\t', "    "),
                                        color,
                                        bold,
                                        italic,
                                    }
                                })
                                .collect(),
                        })
                        .collect();
                    (meta_str, out, truncated || cl.len() > max_lines)
                }
                PreviewBody::Summary { pairs } => {
                    let out: Vec<PreviewLine> = pairs
                        .iter()
                        .map(|(k, v)| PreviewLine {
                            line_no: None,
                            spans: vec![
                                PreviewSpan {
                                    text: format!("{:<12} ", k.as_str()),
                                    color: theme::INK_4,
                                    bold: false,
                                    italic: false,
                                },
                                PreviewSpan {
                                    text: v.as_str().to_owned(),
                                    color: theme::INK_1,
                                    bold: false,
                                    italic: false,
                                },
                            ],
                        })
                        .collect();
                    (meta_str, out, false)
                }
            }
        }
        YaziPreview::Error(e) => ("error".to_owned(), vec![plain(e, theme::DANGER, false)], false),
        _ => (format_size_bytes(size as i64), vec![plain("(empty)".to_owned(), theme::INK_4, true)], false),
    }
}

fn read_dir_entries(dir: &Path, show_hidden: bool) -> Vec<YaziEntry> {
    read_dir_entries_detailed(dir, show_hidden).0
}

fn read_dir_entries_detailed(
    dir: &Path,
    show_hidden: bool,
) -> (Vec<YaziEntry>, usize, blaze_sniff::DirMarkers) {
    let Ok(rd) = fs::read_dir(dir) else {
        return (Vec::new(), 0, blaze_sniff::DirMarkers::default());
    };
    let mut out = Vec::new();
    let mut hidden_count = 0usize;
    let mut markers = blaze_sniff::DirMarkers::default();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        match name.as_str() {
            "Cargo.toml" => markers.has_cargo_toml = true,
            "package.json" => markers.has_package_json = true,
            "pyproject.toml" => markers.has_pyproject = true,
            "go.mod" => markers.has_go_mod = true,
            "flake.nix" => markers.has_flake_nix = true,
            ".git" => markers.has_git = true,
            _ => {}
        }
        if !show_hidden && name.starts_with('.') {
            hidden_count += 1;
            continue;
        }
        if out.len() >= MAX_DIR_ENTRIES {
            continue;
        }
        let meta = entry.metadata().ok();
        let is_dir = meta.as_ref().is_some_and(|m| m.is_dir());
        let size = meta.as_ref().map_or(0, |m| m.len());
        let kind = if is_dir { Some("dir") } else { Some("file") };
        let icon = icon_name_for_file(&name, kind);
        out.push(YaziEntry { name, path: entry.path(), is_dir, size, icon });
    }
    out.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()))
    });
    (out, hidden_count, markers)
}

fn format_dir_summary(
    dir_name: &str,
    entries: &[YaziEntry],
    markers: blaze_sniff::DirMarkers,
) -> String {
    let sniff = blaze_sniff::sniff_dir(dir_name, markers);
    let dirs = entries.iter().filter(|e| e.is_dir).count();
    let files = entries.len().saturating_sub(dirs);
    let d_word = if dirs == 1 { "dir" } else { "dirs" };
    let f_word = if files == 1 { "file" } else { "files" };
    match (dirs, files) {
        (0, 0) => format!("{} · empty", sniff.label),
        (d, 0) => format!("{} · {d} {d_word}", sniff.label),
        (0, f) => format!("{} · {f} {f_word}", sniff.label),
        (d, f) => format!("{} · {d} {d_word} · {f} {f_word}", sniff.label),
    }
}

fn entry_kind_label(entry: &YaziEntry) -> (&'static str, bool) {
    if entry.is_dir {
        let sniff = blaze_sniff::sniff_dir(&entry.name, blaze_sniff::DirMarkers::default());
        let muted = sniff.label == "Build output";
        let lbl = if sniff.label == "Folder" { "" } else { sniff.label };
        (lbl, muted)
    } else {
        let sniff = blaze_sniff::sniff_file(&entry.name, &[]);
        let lbl = match sniff.label {
            "Text" | "Empty" => "",
            other => other,
        };
        (lbl, false)
    }
}

pub(crate) fn token_color(kind: TokenKind) -> ([f32; 4], bool, bool) {
    match kind {
        TokenKind::Plain => (theme::INK_2, false, false),
        TokenKind::Keyword => (theme::rgb(0xc7, 0x9b, 0xf2), false, false),
        TokenKind::Function => (theme::rgb(0xf2, 0xf1, 0xed), false, false),
        TokenKind::Type => (theme::rgb(0x7c, 0xc4, 0xea), false, false),
        TokenKind::String => (theme::rgb(0x7c, 0xc9, 0x8b), false, false),
        TokenKind::Number => (theme::rgb(0xde, 0xc7, 0x92), false, false),
        TokenKind::Comment => (theme::INK_4, false, true),
        TokenKind::Attr => (theme::rgb(0xb4, 0xa2, 0xd8), false, false),
        TokenKind::Key => (theme::rgb(0x9a, 0xc2, 0xe8), false, false),
        TokenKind::Punct => (theme::INK_3, false, false),
    }
}

/// Lay out the native 3-column Miller file browser (`y`) directly in Metal, rendering
/// `blaze-preview`'s structured tables, rendered Markdown, and syntax-highlighted code!
#[allow(clippy::too_many_arguments)]
pub fn layout_yazi_view(
    yazi: &mut YaziState,
    store: &BlockStore,
    prompt: &PromptInfo,
    viewport_w: f32,
    viewport_h: f32,
    m: CellMetrics,
    zoom_pct: u16,
    inline_titlebar: bool,
) -> Scene {
    layout_yazi_inner(yazi, store, prompt, viewport_w, viewport_h, m, zoom_pct, inline_titlebar, true)
}

/// Lay out `y` as a tiled pane (omits global window titlebar).
#[allow(clippy::too_many_arguments)]
pub fn layout_yazi_pane(
    yazi: &mut YaziState,
    store: &BlockStore,
    prompt: &PromptInfo,
    viewport_w: f32,
    viewport_h: f32,
    m: CellMetrics,
    zoom_pct: u16,
) -> Scene {
    layout_yazi_inner(yazi, store, prompt, viewport_w, viewport_h, m, zoom_pct, false, false)
}

#[allow(clippy::too_many_arguments)]
fn layout_yazi_inner(
    yazi: &mut YaziState,
    store: &BlockStore,
    prompt: &PromptInfo,
    viewport_w: f32,
    viewport_h: f32,
    m: CellMetrics,
    zoom_pct: u16,
    inline_titlebar: bool,
    draw_top_bar: bool,
) -> Scene {
    let mut scene = Scene::new();
    let vw = viewport_w.max(240.0);
    let vh = viewport_h.max(200.0);

    let header_h = m.ch + 14.0;
    let footer_h = m.ch + 12.0;
    let body_top = TOP_BAR_H + header_h;
    let body_bot = (vh - footer_h).max(body_top + 40.0);
    let body_h = body_bot - body_top;

    // 1. Path breadcrumb header bar directly below the titlebar.
    scene.round_rect(0.0, TOP_BAR_H, vw, body_top, 0.0, theme::BG_SURFACE, 0.0, [0.0; 4], 0);
    scene.line(0.0, body_top - 0.5, vw, body_top - 0.5, 1.0, theme::LINE, 0);
    let hty = TOP_BAR_H + (header_h - m.ch) * 0.5;
    let hdr_icon_s = m.icon_md();
    scene.svg_icon(14.0, hty + (m.ch - hdr_icon_s) * 0.5, hdr_icon_s, "folder_macos", 0);
    let cwd_str = yazi.cwd.to_string_lossy();
    let max_path_c = ((vw - 280.0) / m.cw).floor().max(16.0) as usize;
    scene.text(14.0 + hdr_icon_s + 7.0, hty, truncate_chars(&cwd_str, max_path_c), theme::INK_1, true, false, 0);

    if yazi.filtering || !yazi.filter.is_empty() {
        let flbl = format!("/{}", yazi.filter);
        let fw = flbl.chars().count() as f32 * m.cw + 14.0;
        let fx0 = vw - 14.0 - fw;
        scene.round_rect(fx0, TOP_BAR_H + 4.0, vw - 14.0, body_top - 4.0, 5.0, theme::BG_CHIP, 1.0, theme::LINE_STRONG, 0);
        scene.text(fx0 + 7.0, hty, flbl, theme::INK_1, false, false, 0);
    }

    // 2. Responsive Miller columns: Parent (16%, hidden below 620px) | Current (30%) | Rich Preview (54%).
    let col1_x0 = 0.0;
    let col1_x1 = if vw < 620.0 { 0.0 } else { (vw * 0.16).clamp(110.0, 210.0) };
    let col2_x0 = col1_x1;
    let col2_max = (vw - 180.0).max(col2_x0 + 120.0).min(vw);
    let col2_min = (col2_x0 + 150.0).min(col2_max);
    let col2_x1 = (col2_x0 + vw * 0.30).clamp(col2_min, col2_max);
    let col3_x0 = col2_x1;
    let col3_x1 = vw;

    if col1_x1 > 0.0 {
        scene.line(col1_x1, body_top, col1_x1, body_bot, 1.0, theme::LINE_FAINT, 0);
    }
    scene.line(col2_x1, body_top, col2_x1, body_bot, 1.0, theme::LINE_FAINT, 0);

    let row_h = m.ch + 6.0;
    let max_vis_rows = ((body_h - 12.0) / row_h).floor().max(1.0) as usize;
    let icon_s = m.icon_md();

    // Column 1: Parent directory (muted context; hidden when narrow so file list + preview stay legible).
    if col1_x1 > 0.0 {
        let clip1 = scene.push_clip(col1_x0, body_top, col1_x1 - 1.0, body_bot, 0.0);
        let p_start = yazi.parent_selected.saturating_sub(max_vis_rows / 2);
        for (slot, entry) in yazi.parent_entries.iter().enumerate().skip(p_start).take(max_vis_rows) {
            let ry = body_top + 6.0 + slot.saturating_sub(p_start) as f32 * row_h;
            let is_cur = slot == yazi.parent_selected;
            if is_cur {
                scene.round_rect(col1_x0 + 6.0, ry, col1_x1 - 6.0, ry + row_h, 4.0, theme::BG_CHIP, 0.0, [0.0; 4], clip1);
            }
            let ty = ry + (row_h - m.ch) * 0.5;
            scene.svg_icon_alpha(col1_x0 + 12.0, ty + (m.ch - icon_s) * 0.5, icon_s, entry.icon, 0.65, clip1);
            let max_c = ((col1_x1 - col1_x0 - 38.0) / m.cw).floor().max(6.0) as usize;
            let col = if is_cur { theme::INK_2 } else { theme::INK_4 };
            scene.text(col1_x0 + 30.0, ty, truncate_chars(&entry.name, max_c), col, false, false, clip1);
        }
    }

    // Column 2: Active directory with crisp monochrome selection rail + sizes.
    let clip2 = scene.push_clip(col2_x0 + 1.0, body_top, col2_x1 - 1.0, body_bot, 0.0);
    let vis = yazi.visible_indices();
    if vis.is_empty() {
        scene.text(col2_x0 + 14.0, body_top + 12.0, "empty directory", theme::INK_4, false, true, clip2);
    } else {
        if yazi.selected >= vis.len() {
            yazi.selected = vis.len() - 1;
        }
        if yazi.selected < yazi.scroll {
            yazi.scroll = yazi.selected;
        } else if yazi.selected >= yazi.scroll + max_vis_rows {
            yazi.scroll = yazi.selected + 1 - max_vis_rows;
        }
        for (slot, &eidx) in vis.iter().enumerate().skip(yazi.scroll).take(max_vis_rows) {
            let entry = &yazi.entries[eidx];
            let ry = body_top + 6.0 + (slot - yazi.scroll) as f32 * row_h;
            let is_sel = slot == yazi.selected;
            if is_sel {
                scene.round_rect(col2_x0 + 6.0, ry, col2_x1 - 6.0, ry + row_h, 5.0, theme::rgb(0x1e, 0x1e, 0x22), 0.0, [0.0; 4], clip2);
                scene.round_rect(col2_x0 + 6.0, ry + 4.0, col2_x0 + 8.0, ry + row_h - 4.0, 1.0, theme::INK_1, 0.0, [0.0; 4], clip2);
            }
            let ty = ry + (row_h - m.ch) * 0.5;
            scene.svg_icon(col2_x0 + 14.0, ty + (m.ch - icon_s) * 0.5, icon_s, entry.icon, clip2);
            let sz_lbl = if entry.is_dir { String::new() } else { format_size_bytes(entry.size as i64) };
            let sz_w = if sz_lbl.is_empty() { 0.0 } else { sz_lbl.len() as f32 * m.cw + 10.0 };
            if !sz_lbl.is_empty() {
                scene.text(col2_x1 - 12.0 - sz_lbl.len() as f32 * m.cw, ty, &sz_lbl, theme::INK_4, false, false, clip2);
            }
            let lbl = if entry.is_dir { format!("{}/", entry.name) } else { entry.name.clone() };
            let max_c = ((col2_x1 - col2_x0 - 44.0 - sz_w) / m.cw).floor().max(6.0) as usize;
            let col = if is_sel { theme::INK_1 } else if entry.is_dir { theme::INK_2 } else { theme::INK_3 };
            scene.text(col2_x0 + 34.0, ty, truncate_chars(&lbl, max_c), col, is_sel, false, clip2);
        }
    }

    // Column 3: Rich `blaze-preview` pane (Tables | KeyValues | Rendered Markdown | Highlighted Code | Summary).
    yazi.preview_split_x = col3_x0;
    let clip3 = scene.push_clip(col3_x0 + 1.0, body_top, col3_x1, body_bot, 0.0);
    let px = col3_x0 + 18.0;
    let pw = (col3_x1 - px - 18.0).max(100.0);
    match &yazi.preview {
        YaziPreview::Empty => {
            yazi.preview_max_scroll = 0;
            scene.text(px, body_top + 14.0, "no selection", theme::INK_4, false, true, clip3);
        }
        YaziPreview::Directory { meta, items } => {
            let hdr_h = m.ch + 10.0;
            scene.ui_small(px, body_top + 8.0, meta.as_str(), theme::INK_4, false, false, clip3);
            scene.line(px, body_top + hdr_h + 2.0, col3_x1 - 14.0, body_top + hdr_h + 2.0, 1.0, theme::LINE_FAINT, clip3);
            let list_top = body_top + hdr_h + 6.0;
            let dir_vis_rows = ((body_bot - list_top - 6.0) / row_h).floor().max(1.0) as usize;
            let max_s = items.len().saturating_sub(dir_vis_rows);
            yazi.preview_max_scroll = max_s;
            let sc = yazi.preview_scroll.min(max_s);
            yazi.preview_scroll = sc;
            if items.is_empty() {
                scene.ui_small(px, list_top + 8.0, "empty directory", theme::INK_4, false, true, clip3);
            } else {
                let sz_col_w = 9.0 * m.cw;
                let show_kind = pw >= 290.0;
                let kind_col_w = if show_kind { 13.0 * m.cw } else { 0.0 };
                for (i, sub) in items.iter().skip(sc).take(dir_vis_rows).enumerate() {
                    let ry = list_top + i as f32 * row_h;
                    let ty = ry + (row_h - m.ch) * 0.5;
                    let (kind_lbl, muted) = entry_kind_label(sub);
                    let alpha = if muted { 0.72 } else { 1.0 };
                    scene.svg_icon_alpha(px, ty + (m.ch - icon_s) * 0.5, icon_s, sub.icon, alpha, clip3);
                    let lbl = if sub.is_dir { format!("{}/", sub.name) } else { sub.name.clone() };
                    let name_w = (pw - 24.0 - sz_col_w - kind_col_w - 8.0).max(8.0 * m.cw);
                    let max_c = (name_w / m.cw).floor().max(8.0) as usize;
                    let name_col = if muted {
                        theme::INK_4
                    } else if sub.is_dir {
                        theme::INK_1
                    } else {
                        theme::INK_2
                    };
                    scene.text(px + 20.0, ty, truncate_chars(&lbl, max_c), name_col, false, false, clip3);
                    if show_kind && !kind_lbl.is_empty() {
                        let kx = col3_x1 - 18.0 - sz_col_w - kind_col_w;
                        scene.ui_small(kx, ty, truncate_chars(kind_lbl, 14), theme::INK_4, false, false, clip3);
                    }
                    if !sub.is_dir {
                        let sz = format_size_bytes(sub.size as i64);
                        let sw = sz.chars().count() as f32 * m.cw;
                        scene.text(col3_x1 - 18.0 - sw, ty, sz, theme::INK_3, false, false, clip3);
                    }
                }
                if max_s > 0 {
                    draw_mini_scrollbar(&mut scene, col3_x1 - 9.0, list_top, body_bot - 8.0, sc, max_s, dir_vis_rows, items.len(), clip3);
                }
            }
        }
        YaziPreview::Rich(doc) => {
            let (_, max_s) = render_rich_preview_scrolled(
                &mut scene,
                doc,
                Some(yazi.cwd.as_path()),
                px,
                body_top + 10.0,
                pw,
                body_bot - 8.0,
                m,
                clip3,
                yazi.preview_scroll,
            );
            yazi.preview_max_scroll = max_s;
            if yazi.preview_scroll > max_s {
                yazi.preview_scroll = max_s;
            }
        }
        YaziPreview::Error(err) => {
            yazi.preview_max_scroll = 0;
            scene.text(px, body_top + 14.0, err, theme::DANGER, false, false, clip3);
        }
    }

    // 3. Minimal status footer bar with `blaze-sniff` type tag!
    scene.round_rect(0.0, body_bot, vw, vh, 0.0, theme::BG_SURFACE, 0.0, [0.0; 4], 0);
    scene.line(0.0, body_bot, vw, body_bot, 1.0, theme::LINE, 0);
    let fty = body_bot + (footer_h - m.ch) * 0.5;
    let kind_badge = match &yazi.preview {
        YaziPreview::Rich(doc) => {
            let mut s = String::new();
            let _ = doc.sniff.format_label(&mut s);
            format!(" · {s}")
        }
        YaziPreview::Directory { meta, .. } => format!(" · {meta}"),
        _ => String::new(),
    };
    let pos_lbl = if vis.is_empty() {
        format!("y · 0/0{kind_badge}")
    } else {
        format!("y · {}/{}{kind_badge}", yazi.selected + 1, vis.len())
    };
    scene.ui_small(14.0, fty, pos_lbl, theme::INK_2, true, false, 0);
    let hint = "hjkl move  ·  J/K scroll  ·  / filter  ·  . hidden  ·  space insert  ·  q exit";
    let hw = m.measure(hint, TextFace::UiSmall);
    if vw - 14.0 - hw > 220.0 {
        scene.ui_small(vw - 14.0 - hw, fty, hint, theme::INK_4, false, false, 0);
    }

    if draw_top_bar {
        layout_top_bar(&mut scene, store, prompt, vw, m, zoom_pct, inline_titlebar);
    }
    scene
}

#[allow(clippy::too_many_arguments)]
fn draw_mini_scrollbar(
    scene: &mut Scene,
    x0: f32,
    y0: f32,
    y1: f32,
    scroll: usize,
    max_scroll: usize,
    visible: usize,
    total: usize,
    clip: u32,
) {
    if max_scroll == 0 || y1 <= y0 + 20.0 {
        return;
    }
    let track_h = y1 - y0;
    let w = 3.5;
    scene.round_rect(x0, y0, x0 + w, y1, 1.75, theme::BG_TRACK, 0.0, [0.0; 4], clip);
    let frac = (visible as f32 / total.max(1) as f32).clamp(0.10, 1.0);
    let thumb_h = (track_h * frac).max(18.0);
    let pos_frac = (scroll.min(max_scroll) as f32) / (max_scroll as f32);
    let ty0 = y0 + (track_h - thumb_h) * pos_frac;
    scene.over_round_rect(x0, ty0, x0 + w, ty0 + thumb_h, 1.75, theme::INK_4, clip);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_rich_preview(
    scene: &mut Scene,
    doc: &PreviewDoc<'_>,
    base_dir: Option<&Path>,
    x0: f32,
    y0: f32,
    w: f32,
    max_y: f32,
    m: CellMetrics,
    clip: u32,
) -> f32 {
    render_rich_preview_scrolled(scene, doc, base_dir, x0, y0, w, max_y, m, clip, 0).0
}

#[allow(clippy::too_many_arguments)]
fn render_rich_preview_scrolled(
    scene: &mut Scene,
    doc: &PreviewDoc<'_>,
    base_dir: Option<&Path>,
    x0: f32,
    y0: f32,
    w: f32,
    max_y: f32,
    m: CellMetrics,
    clip: u32,
    scroll: usize,
) -> (f32, usize) {
    match &doc.body {
        PreviewBody::Table(tbl) => {
            let (end_y, max_s) = render_preview_table(scene, tbl, x0, y0, w, max_y, m, clip, scroll);
            (end_y - y0, max_s)
        }
        PreviewBody::KeyValues(kv) => {
            let (end_y, max_s) = render_preview_kv(scene, kv, x0, y0, w, max_y, m, clip, scroll);
            (end_y - y0, max_s)
        }
        PreviewBody::Markdown { blocks, truncated } => {
            let slice = blocks.as_slice();
            let est_vis = (((max_y - y0) / (m.ch + 5.0)).floor().max(1.0) as usize).min(slice.len());
            let max_s = slice.len().saturating_sub(est_vis);
            let sc = scroll.min(max_s);
            let end_y = render_preview_markdown(
                scene,
                &slice[sc..],
                *truncated,
                base_dir,
                x0,
                y0,
                w - 8.0,
                max_y,
                m,
                clip,
            );
            if max_s > 0 && scroll > 0 {
                draw_mini_scrollbar(scene, x0 + w + 4.0, y0, max_y, sc, max_s, est_vis, slice.len(), clip);
            }
            (end_y - y0, max_s)
        }
        PreviewBody::Code { lang, lines, truncated } => {
            let mut y = y0;
            scene.text(x0, y, *lang, theme::INK_3, true, false, clip);
            let sz = format_size_bytes(doc.file_size as i64);
            scene.text(x0 + w - sz.len() as f32 * m.cw, y, sz, theme::INK_4, false, false, clip);
            y += m.ch + 6.0;
            scene.line(x0, y, x0 + w, y, 1.0, theme::LINE_FAINT, clip);
            y += 8.0;
            let slice = lines.as_slice();
            let vis = (((max_y - y) / (m.ch + 2.0)).floor().max(1.0) as usize).min(slice.len());
            let max_s = slice.len().saturating_sub(vis);
            let sc = scroll.min(max_s);
            let end_y = render_code_lines(scene, &slice[sc..], x0, y, w - 8.0, max_y, m, clip, true);
            if max_s > 0 {
                draw_mini_scrollbar(scene, x0 + w + 4.0, y, max_y, sc, max_s, vis, slice.len(), clip);
            }
            if *truncated && sc == max_s && end_y + m.ch <= max_y {
                scene.text(x0 + 4.0 * m.cw, end_y + 2.0, "… truncated", theme::INK_4, false, true, clip);
                ((end_y + m.ch + 4.0) - y0, max_s)
            } else {
                (end_y - y0, max_s)
            }
        }
        PreviewBody::Summary { pairs } => {
            let mut y = y0;
            let mut lbl = String::new();
            let _ = doc.sniff.format_label(&mut lbl);
            scene.text(x0, y, &lbl, theme::INK_1, true, false, clip);
            y += m.ch + 8.0;
            scene.line(x0, y, x0 + w, y, 1.0, theme::LINE_FAINT, clip);
            y += 10.0;
            for (k, v) in pairs.iter() {
                if y + m.ch > max_y {
                    break;
                }
                scene.text(x0, y, k.as_str(), theme::INK_4, false, false, clip);
                scene.text(x0 + 14.0 * m.cw, y, v.as_str(), theme::INK_1, false, false, clip);
                y += m.ch + 6.0;
            }
            (y - y0, 0)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_preview_table(
    scene: &mut Scene,
    tbl: &PreviewTable<'_>,
    x0: f32,
    mut y: f32,
    w: f32,
    max_y: f32,
    m: CellMetrics,
    clip: u32,
    scroll: usize,
) -> (f32, usize) {
    scene.text(x0, y, tbl.title.as_str(), theme::INK_1, true, false, clip);
    let sub_w = tbl.subtitle.chars().count() as f32 * m.cw;
    scene.text(x0 + w - sub_w, y, tbl.subtitle.as_str(), theme::INK_4, false, false, clip);
    y += m.ch + 6.0;
    scene.line(x0, y, x0 + w, y, 1.0, theme::LINE_FAINT, clip);
    y += 7.0;

    let ncols = tbl.headers.len().max(1);
    let idx_w = 3.5 * m.cw;
    let inner_w = (w - idx_w - 8.0).max(80.0);
    let col_gap = 14.0;
    let mut widths = vec![0usize; ncols];
    for (c, h) in tbl.headers.iter().enumerate() {
        widths[c] = h.chars().count().max(4);
    }
    for row in tbl.rows.iter().take(128) {
        for (c, cell) in row.iter().enumerate().take(ncols) {
            widths[c] = widths[c].max(cell.chars().count().min(32));
        }
    }
    let total_px: f32 = widths.iter().map(|&n| n as f32 * m.cw + col_gap).sum();
    let scale = if total_px > inner_w && total_px > 0.0 { inner_w / total_px } else { 1.0 };
    let col_px: Vec<f32> = widths.iter().map(|&n| (n as f32 * m.cw + col_gap) * scale).collect();

    // Sticky header row
    scene.text(x0, y, "#", theme::INK_4, false, false, clip);
    let mut cx = x0 + idx_w;
    for (c, h) in tbl.headers.iter().enumerate() {
        let max_c = ((col_px[c] - 6.0) / m.cw).floor().max(3.0) as usize;
        scene.text(cx, y, truncate_chars(h.as_str(), max_c), theme::INK_3, false, false, clip);
        cx += col_px[c];
    }
    y += m.ch + 5.0;
    scene.line(x0, y, x0 + w, y, 1.0, theme::LINE, clip);
    y += 5.0;

    let rows_top = y;
    let row_h = m.ch + 5.0;
    let vis_rows = (((max_y - rows_top) / row_h).floor().max(1.0) as usize).min(tbl.rows.len());
    let max_s = tbl.rows.len().saturating_sub(vis_rows);
    let sc = scroll.min(max_s);

    for (i, row) in tbl.rows.iter().enumerate().skip(sc).take(vis_rows) {
        scene.text(x0, y + 1.0, format!("{i}"), theme::INK_4, false, false, clip);
        let mut rx = x0 + idx_w;
        for (c, cell) in row.iter().enumerate().take(ncols) {
            let max_c = ((col_px[c] - 6.0) / m.cw).floor().max(3.0) as usize;
            let col = if c == 0 {
                theme::INK_1
            } else if c == 1 {
                theme::rgb(0x7c, 0xc9, 0x8b)
            } else {
                theme::INK_3
            };
            scene.text(rx, y + 1.0, truncate_chars(cell.as_str(), max_c), col, false, false, clip);
            rx += col_px[c];
        }
        y += row_h;
    }
    if max_s > 0 {
        draw_mini_scrollbar(scene, x0 + w + 4.0, rows_top, y, sc, max_s, vis_rows, tbl.rows.len(), clip);
    }
    if tbl.total_rows > tbl.rows.len() && sc == max_s && y + m.ch <= max_y {
        let more = format!("… +{} more", tbl.total_rows - tbl.rows.len());
        scene.text(x0 + idx_w, y + 2.0, more, theme::INK_4, false, true, clip);
        y += m.ch + 4.0;
    }
    (y, max_s)
}

#[allow(clippy::too_many_arguments)]
fn render_preview_kv(
    scene: &mut Scene,
    kv: &PreviewKv<'_>,
    x0: f32,
    mut y: f32,
    w: f32,
    max_y: f32,
    m: CellMetrics,
    clip: u32,
    scroll: usize,
) -> (f32, usize) {
    scene.text(x0, y, kv.title.as_str(), theme::INK_1, true, false, clip);
    let sub_w = kv.subtitle.chars().count() as f32 * m.cw;
    scene.text(x0 + w - sub_w, y, kv.subtitle.as_str(), theme::INK_4, false, false, clip);
    y += m.ch + 6.0;
    scene.line(x0, y, x0 + w, y, 1.0, theme::LINE_FAINT, clip);
    y += 8.0;

    let key_w = (kv.pairs.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(8).clamp(8, 20) as f32 + 3.0) * m.cw;
    let val_max_c = ((w - key_w) / m.cw).floor().max(10.0) as usize;
    for (k, v) in kv.pairs.iter() {
        if y + m.ch > max_y {
            return (y, 0);
        }
        scene.text(x0, y, k.as_str(), theme::INK_3, false, false, clip);
        scene.text(x0 + key_w, y, truncate_chars(v.as_str(), val_max_c), theme::INK_1, false, false, clip);
        y += m.ch + 4.0;
    }
    let mut max_s = 0usize;
    if let Some(tbl) = &kv.table
        && y + m.ch * 3.0 < max_y
    {
        y += 10.0;
        let (end_y, ms) = render_preview_table(scene, tbl, x0, y, w, max_y, m, clip, scroll);
        y = end_y;
        max_s = ms;
    }
    (y, max_s)
}

#[allow(clippy::too_many_arguments)]
fn render_preview_markdown(
    scene: &mut Scene,
    blocks: &[MdBlock<'_>],
    truncated: bool,
    base_dir: Option<&Path>,
    x0: f32,
    mut y: f32,
    w: f32,
    max_y: f32,
    m: CellMetrics,
    clip: u32,
) -> f32 {
    let content_w = w.clamp(120.0, 920.0);
    for blk in blocks {
        if y + m.ch > max_y {
            break;
        }
        match blk {
            MdBlock::Blank => {
                y += m.ch * 0.55;
            }
            MdBlock::Rule => {
                let ry = y + m.ch * 0.45;
                scene.line(x0, ry, x0 + content_w, ry, 1.0, theme::LINE, clip);
                y += m.ch * 0.95;
            }
            MdBlock::Heading { level, align, spans } => {
                if y > 60.0 {
                    y += if *level <= 2 { 8.0 } else { 5.0 };
                }
                let col = if *level <= 2 { [1.0, 1.0, 1.0, 1.0] } else { theme::INK_1 };
                let face = if *level <= 2 { TextFace::Heading } else { TextFace::Ui };
                let used = render_md_spans_wrapped(
                    scene,
                    spans.as_slice(),
                    x0,
                    y,
                    content_w,
                    max_y,
                    col,
                    true,
                    face,
                    *align,
                    m,
                    clip,
                );
                y += used + 3.0;
                if *level <= 2 && *align == TextAlign::Left {
                    scene.line(x0, y - 1.0, x0 + content_w, y - 1.0, 1.0, theme::LINE_FAINT, clip);
                    y += 6.0;
                }
            }
            MdBlock::Paragraph { align, spans } => {
                let used = render_md_spans_wrapped(
                    scene,
                    spans.as_slice(),
                    x0,
                    y,
                    content_w,
                    max_y,
                    theme::INK_2,
                    false,
                    TextFace::Ui,
                    *align,
                    m,
                    clip,
                );
                y += used + 4.0;
            }
            MdBlock::Image { src, alt, width, height, align } => {
                y = render_md_image(
                    scene,
                    src.as_str(),
                    alt.as_str(),
                    *width,
                    *height,
                    *align,
                    base_dir,
                    x0,
                    y,
                    content_w,
                    max_y,
                    m,
                    clip,
                );
            }
            MdBlock::Bullet { indent, marker, spans } => {
                let ix = x0 + (*indent as f32) * 16.0;
                scene.ui_text(ix, y, marker.as_str(), theme::INK_3, true, false, clip);
                let bx = ix + m.measure(marker.as_str(), TextFace::Ui) + 7.0;
                let avail_w = (x0 + content_w - bx).max(80.0);
                let used = render_md_spans_wrapped(
                    scene,
                    spans.as_slice(),
                    bx,
                    y,
                    avail_w,
                    max_y,
                    theme::INK_2,
                    false,
                    TextFace::Ui,
                    TextAlign::Left,
                    m,
                    clip,
                );
                y += used + 4.0;
            }
            MdBlock::Quote { spans } => {
                let start_y = y;
                let used = render_md_spans_wrapped(
                    scene,
                    spans.as_slice(),
                    x0 + 13.0,
                    y,
                    (content_w - 15.0).max(80.0),
                    max_y,
                    theme::INK_3,
                    false,
                    TextFace::Ui,
                    TextAlign::Left,
                    m,
                    clip,
                );
                scene.over_round_rect(
                    x0,
                    start_y + 1.0,
                    x0 + 2.5,
                    (start_y + used - 2.0).max(start_y + m.ch - 2.0),
                    1.25,
                    theme::LINE_STRONG,
                    clip,
                );
                y += used + 5.0;
            }
            MdBlock::CodeFence { lang, lines } => {
                let box_h = (lines.len().max(1) as f32 * (m.ch + 2.0) + 14.0).min((max_y - y).max(20.0));
                scene.round_rect(
                    x0,
                    y,
                    x0 + content_w,
                    y + box_h,
                    7.0,
                    theme::rgb(0x15, 0x15, 0x18),
                    1.0,
                    theme::LINE_FAINT,
                    clip,
                );
                if !lang.is_empty() {
                    let lw = m.measure(lang.as_str(), TextFace::UiSmall);
                    scene.ui_small(
                        x0 + content_w - 12.0 - lw,
                        y + 5.0,
                        lang.as_str(),
                        theme::INK_4,
                        false,
                        false,
                        clip,
                    );
                }
                render_code_lines(
                    scene,
                    lines.as_slice(),
                    x0 + 12.0,
                    y + 7.0,
                    content_w - 24.0,
                    y + box_h - 4.0,
                    m,
                    clip,
                    false,
                );
                y += box_h + 8.0;
            }
            MdBlock::Table(tbl) => {
                y = render_md_table(scene, tbl, x0, y, content_w, max_y, m, clip);
            }
            MdBlock::Details { summary, open } => {
                let tri = if *open { "▾" } else { "▸" };
                scene.ui_text(x0, y, tri, theme::INK_3, true, false, clip);
                let used = render_md_spans_wrapped(
                    scene,
                    summary.as_slice(),
                    x0 + 15.0,
                    y,
                    (content_w - 15.0).max(80.0),
                    max_y,
                    theme::INK_1,
                    true,
                    TextFace::Ui,
                    TextAlign::Left,
                    m,
                    clip,
                );
                y += used + 4.0;
            }
        }
    }
    if truncated && y + m.ch <= max_y {
        scene.ui_small(x0, y + 2.0, "… more content", theme::INK_4, false, true, clip);
        y += m.ch + 4.0;
    }
    y
}

#[allow(clippy::too_many_arguments)]
fn render_md_image(
    scene: &mut Scene,
    src: &str,
    alt: &str,
    attr_w: Option<u32>,
    attr_h: Option<u32>,
    align: TextAlign,
    base_dir: Option<&Path>,
    x0: f32,
    y: f32,
    max_w: f32,
    max_y: f32,
    m: CellMetrics,
    clip: u32,
) -> f32 {
    let is_remote = src.starts_with("http://") || src.starts_with("https://");
    let resolved = if !is_remote && !src.is_empty() {
        let p = Path::new(src);
        let full = if p.is_absolute() {
            p.to_path_buf()
        } else if let Some(base) = base_dir {
            base.join(p)
        } else {
            p.to_path_buf()
        };
        full.is_file().then_some(full)
    } else {
        None
    };

    let is_raster = resolved.as_ref().is_some_and(|p| {
        p.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|ext| {
                matches!(
                    ext.to_ascii_lowercase().as_str(),
                    "webp" | "png" | "jpg" | "jpeg" | "gif" | "heic" | "bmp" | "tif" | "tiff"
                )
            })
    });

    if let Some(full) = resolved.as_ref().filter(|_| is_raster) {
        let probed = crate::image_atlas::probe_image_size(full);
        let (nat_w, nat_h) = match (attr_w, attr_h, probed) {
            (Some(w), Some(h), _) => (w as f32, h as f32),
            (Some(w), None, Some((pw, ph))) if pw > 0 => (w as f32, w as f32 * (ph as f32 / pw as f32)),
            (None, Some(h), Some((pw, ph))) if ph > 0 => (h as f32 * (pw as f32 / ph as f32), h as f32),
            (_, _, Some((pw, ph))) => (pw as f32, ph as f32),
            (Some(w), None, None) => (w as f32, (w as f32) * 0.5),
            _ => (320.0, 160.0),
        };
        let mut dw = nat_w.min(max_w);
        let mut dh = if nat_w > 0.0 { nat_h * (dw / nat_w) } else { 160.0 };
        let max_h = (max_y - y - 8.0).clamp(48.0, 360.0);
        if dh > max_h && dh > 0.0 {
            dw *= max_h / dh;
            dh = max_h;
        }
        let ix = match align {
            TextAlign::Left => x0,
            TextAlign::Center => x0 + ((max_w - dw) * 0.5).max(0.0),
            TextAlign::Right => x0 + (max_w - dw).max(0.0),
        };
        scene.image(ix, y + 2.0, dw, dh, full.to_string_lossy().into_owned(), clip);
        return y + dh + 10.0;
    }

    // Crisp dark media pill for SVG charts / remote images / missing assets.
    let caption = if !alt.is_empty() {
        alt
    } else if !src.is_empty() {
        src
    } else {
        "image"
    };
    let short_src = Path::new(src)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(src);
    let badge_w = (m.measure(caption, TextFace::UiSmall) + 38.0).clamp(140.0, max_w);
    let badge_h = m.ch + 14.0;
    let bx = match align {
        TextAlign::Left => x0,
        TextAlign::Center => x0 + ((max_w - badge_w) * 0.5).max(0.0),
        TextAlign::Right => x0 + (max_w - badge_w).max(0.0),
    };
    scene.round_rect(
        bx,
        y + 2.0,
        bx + badge_w,
        y + 2.0 + badge_h,
        6.0,
        theme::rgb(0x16, 0x17, 0x1c),
        1.0,
        theme::LINE,
        clip,
    );
    let ico_s = m.icon_sm();
    scene.svg_icon(bx + 9.0, y + 2.0 + (badge_h - ico_s) * 0.5, ico_s, "image_dark", clip);
    let label_max = ((badge_w - 34.0) / m.measure("n", TextFace::UiSmall).max(4.5)).floor().max(8.0) as usize;
    let shown = truncate_chars(if alt.len() > 80 { short_src } else { caption }, label_max);
    scene.ui_small(bx + 28.0, y + 9.0, shown, theme::INK_2, false, false, clip);
    y + badge_h + 8.0
}

#[allow(clippy::too_many_arguments)]
fn render_md_table(
    scene: &mut Scene,
    tbl: &MdTable<'_>,
    x0: f32,
    y0: f32,
    max_w: f32,
    max_y: f32,
    m: CellMetrics,
    clip: u32,
) -> f32 {
    let ncols = tbl
        .headers
        .len()
        .max(tbl.rows.iter().map(|r| r.len()).max().unwrap_or(0));
    if ncols == 0 {
        return y0;
    }

    // Compute natural width of every column from headers and rows.
    let mut col_w = vec![48.0f32; ncols];
    let measure_cell = |spans: &[MdSpan<'_>], bold_hdr: bool| -> f32 {
        let mut total = 0.0f32;
        for sp in spans {
            let (face, bold) = span_face_bold(sp.style, bold_hdr, TextFace::Ui);
            total += m.measure_styled(sp.text.as_str(), face, bold);
            if matches!(sp.style, MdStyle::Code | MdStyle::Kbd) {
                total += 8.0;
            }
        }
        total + 22.0
    };
    for (ci, h) in tbl.headers.iter().enumerate() {
        if ci < ncols {
            col_w[ci] = col_w[ci].max(measure_cell(h.as_slice(), true));
        }
    }
    for row in tbl.rows.iter() {
        for (ci, c) in row.iter().enumerate() {
            if ci < ncols {
                col_w[ci] = col_w[ci].max(measure_cell(c.as_slice(), false));
            }
        }
    }
    let sum_w: f32 = col_w.iter().sum();
    let table_w = sum_w.min(max_w);
    if sum_w > max_w && sum_w > 0.0 {
        let ratio = max_w / sum_w;
        for w in &mut col_w {
            *w = (*w * ratio).max(40.0);
        }
    }

    let row_h = m.ch + 9.0;
    let has_hdr = !tbl.headers.is_empty();
    let vis_rows = tbl
        .rows
        .len()
        .min((((max_y - y0 - if has_hdr { row_h } else { 0.0 }) / row_h).floor().max(0.0)) as usize);
    let total_h = (if has_hdr { row_h } else { 0.0 }) + (vis_rows as f32) * row_h + 2.0;

    // Outer table frame.
    scene.round_rect(
        x0,
        y0,
        x0 + table_w,
        y0 + total_h,
        7.0,
        theme::rgb(0x13, 0x14, 0x18),
        1.0,
        theme::LINE,
        clip,
    );

    let mut y = y0 + 1.0;
    if has_hdr {
        scene.round_rect(
            x0 + 1.0,
            y,
            x0 + table_w - 1.0,
            y + row_h,
            6.0,
            theme::rgb(0x1a, 0x1b, 0x22),
            0.0,
            [0.0; 4],
            clip,
        );
        let mut cx = x0;
        for (ci, h) in tbl.headers.iter().enumerate().take(ncols) {
            let align = tbl.alignments.get(ci).copied().unwrap_or(TextAlign::Left);
            render_md_spans_single_line(
                scene,
                h.as_slice(),
                cx + 10.0,
                y + (row_h - m.ch) * 0.5,
                (col_w[ci] - 20.0).max(20.0),
                theme::INK_1,
                true,
                TextFace::Ui,
                align,
                m,
                clip,
            );
            cx += col_w[ci];
        }
        y += row_h;
        scene.line(x0 + 1.0, y, x0 + table_w - 1.0, y, 1.0, theme::LINE, clip);
    }

    for (ri, row) in tbl.rows.iter().take(vis_rows).enumerate() {
        if ri > 0 {
            scene.line(x0 + 1.0, y, x0 + table_w - 1.0, y, 1.0, theme::LINE_FAINT, clip);
        }
        let mut cx = x0;
        for (ci, cell) in row.iter().enumerate().take(ncols) {
            let align = tbl.alignments.get(ci).copied().unwrap_or(TextAlign::Left);
            render_md_spans_single_line(
                scene,
                cell.as_slice(),
                cx + 10.0,
                y + (row_h - m.ch) * 0.5,
                (col_w[ci] - 20.0).max(20.0),
                theme::INK_2,
                false,
                TextFace::Ui,
                align,
                m,
                clip,
            );
            cx += col_w[ci];
        }
        y += row_h;
    }

    y0 + total_h + 8.0
}

fn span_face_bold(style: MdStyle, force_bold: bool, prose_face: TextFace) -> (TextFace, bool) {
    match style {
        MdStyle::Code | MdStyle::Kbd => (TextFace::Mono, false),
        MdStyle::Bold | MdStyle::BoldItalic => (prose_face, true),
        _ => (prose_face, force_bold),
    }
}

struct WrappedRun {
    text: String,
    style: MdStyle,
    width: f32,
    pad_l: f32,
    pad_r: f32,
}

#[allow(clippy::too_many_arguments)]
fn render_md_spans_wrapped(
    scene: &mut Scene,
    spans: &[MdSpan<'_>],
    x0: f32,
    y0: f32,
    max_w: f32,
    max_y: f32,
    base_col: [f32; 4],
    force_bold: bool,
    prose_face: TextFace,
    align: TextAlign,
    m: CellMetrics,
    clip: u32,
) -> f32 {
    let line_h = (m.line_h(prose_face) + 3.5).max(m.ch + 2.0);
    if spans.is_empty() {
        return line_h;
    }

    // Break spans into word/whitespace pieces and pack them cleanly into wrapped lines.
    let mut lines: Vec<Vec<WrappedRun>> = vec![Vec::new()];
    let mut cur_w = 0.0f32;

    for sp in spans {
        let is_chip = matches!(sp.style, MdStyle::Code | MdStyle::Kbd);
        let (face, bold) = span_face_bold(sp.style, force_bold, prose_face);
        if is_chip {
            let sw = m.measure_styled(sp.text.as_str(), face, bold);
            let pad_l = 3.5;
            let pad_r = 3.5;
            let total = sw + pad_l + pad_r;
            if cur_w > 0.0 && cur_w + total > max_w {
                lines.push(Vec::new());
                cur_w = 0.0;
            }
            lines.last_mut().unwrap().push(WrappedRun {
                text: sp.text.as_str().to_owned(),
                style: sp.style,
                width: sw,
                pad_l,
                pad_r,
            });
            cur_w += total;
            continue;
        }

        // Tokenize prose spans into words + preserving spaces so inline links/bold never glue together.
        let s = sp.text.as_str();
        let mut pos = 0usize;
        let bytes = s.as_bytes();
        while pos < bytes.len() {
            let start = pos;
            let is_ws = bytes[pos].is_ascii_whitespace();
            while pos < bytes.len() && bytes[pos].is_ascii_whitespace() == is_ws {
                pos += 1;
            }
            let piece = &s[start..pos];
            if is_ws && cur_w == 0.0 {
                continue; // Drop leading spaces on freshly wrapped lines.
            }
            let pw = m.measure_styled(piece, face, bold);
            if !is_ws && cur_w > 0.0 && cur_w + pw > max_w {
                // Trim trailing whitespace on the line being closed.
                if let Some(last_line) = lines.last_mut()
                    && let Some(last_run) = last_line.last()
                    && last_run.text.trim().is_empty()
                {
                    last_line.pop();
                }
                lines.push(Vec::new());
                cur_w = 0.0;
            }
            let cur_line = lines.last_mut().unwrap();
            if let Some(prev) = cur_line.last_mut()
                && prev.style == sp.style
                && prev.pad_l == 0.0
                && prev.pad_r == 0.0
            {
                prev.text.push_str(piece);
                prev.width += pw;
            } else {
                cur_line.push(WrappedRun {
                    text: piece.to_owned(),
                    style: sp.style,
                    width: pw,
                    pad_l: 0.0,
                    pad_r: 0.0,
                });
            }
            cur_w += pw;
        }
    }

    let mut y = y0;
    let mut drawn = 0usize;
    for line in &lines {
        if y + m.ch > max_y {
            break;
        }
        let line_w: f32 = line.iter().map(|r| r.pad_l + r.width + r.pad_r).sum();
        let mut cx = match align {
            TextAlign::Left => x0,
            TextAlign::Center => x0 + ((max_w - line_w) * 0.5).max(0.0),
            TextAlign::Right => x0 + (max_w - line_w).max(0.0),
        };
        for run in line {
            cx += run.pad_l;
            draw_styled_md_run(scene, &run.text, run.style, run.width, cx, y, base_col, force_bold, prose_face, m, clip);
            cx += run.width + run.pad_r;
        }
        y += line_h;
        drawn += 1;
    }

    (drawn.max(1) as f32) * line_h
}

#[allow(clippy::too_many_arguments)]
fn render_md_spans_single_line(
    scene: &mut Scene,
    spans: &[MdSpan<'_>],
    x0: f32,
    y: f32,
    max_w: f32,
    base_col: [f32; 4],
    force_bold: bool,
    prose_face: TextFace,
    align: TextAlign,
    m: CellMetrics,
    clip: u32,
) {
    let mut total_w = 0.0f32;
    for sp in spans {
        let (face, bold) = span_face_bold(sp.style, force_bold, prose_face);
        total_w += m.measure_styled(sp.text.as_str(), face, bold);
        if matches!(sp.style, MdStyle::Code | MdStyle::Kbd) {
            total_w += 8.0;
        }
    }
    let mut cx = match align {
        TextAlign::Left => x0,
        TextAlign::Center => x0 + ((max_w - total_w) * 0.5).max(0.0),
        TextAlign::Right => x0 + (max_w - total_w).max(0.0),
    };
    let x_end = x0 + max_w;
    for sp in spans {
        let rem_w = x_end - cx;
        if rem_w < m.cw * 1.5 {
            break;
        }
        let (face, bold) = span_face_bold(sp.style, force_bold, prose_face);
        let pad = if matches!(sp.style, MdStyle::Code | MdStyle::Kbd) { 4.0 } else { 0.0 };
        let avg_cw = m.measure("n", face).max(4.0);
        let max_c = ((rem_w - pad * 2.0) / avg_cw).floor().max(2.0) as usize;
        let txt = truncate_chars(sp.text.as_str(), max_c);
        let sw = m.measure_styled(&txt, face, bold);
        cx += pad;
        draw_styled_md_run(scene, &txt, sp.style, sw, cx, y, base_col, force_bold, prose_face, m, clip);
        cx += sw + pad;
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_styled_md_run(
    scene: &mut Scene,
    txt: &str,
    style: MdStyle,
    sw: f32,
    cx: f32,
    y: f32,
    base_col: [f32; 4],
    force_bold: bool,
    prose_face: TextFace,
    m: CellMetrics,
    clip: u32,
) {
    match style {
        MdStyle::Plain => {
            scene.text_with_face(cx, y, txt, base_col, force_bold, false, prose_face, clip);
        }
        MdStyle::Bold => {
            scene.text_with_face(cx, y, txt, theme::INK_1, true, false, prose_face, clip);
        }
        MdStyle::Italic => {
            scene.text_with_face(cx, y, txt, theme::INK_2, force_bold, true, prose_face, clip);
        }
        MdStyle::BoldItalic => {
            scene.text_with_face(cx, y, txt, theme::INK_1, true, true, prose_face, clip);
        }
        MdStyle::Code => {
            scene.round_rect(
                cx - 3.0,
                y,
                cx + sw + 3.0,
                y + m.ch,
                4.0,
                theme::rgb(0x1f, 0x20, 0x27),
                1.0,
                theme::rgba(0x3a, 0x3c, 0x48, 0.55),
                clip,
            );
            scene.text(cx, y, txt, theme::rgb(0x7c, 0xc9, 0x8b), false, false, clip);
        }
        MdStyle::Kbd => {
            scene.round_rect(
                cx - 3.5,
                y,
                cx + sw + 3.5,
                y + m.ch,
                4.0,
                theme::rgb(0x25, 0x26, 0x2d),
                1.0,
                theme::rgb(0x46, 0x48, 0x54),
                clip,
            );
            scene.text(cx, y, txt, theme::INK_1, true, false, clip);
        }
        MdStyle::Strike => {
            scene.text_with_face(cx, y, txt, theme::INK_4, force_bold, false, prose_face, clip);
            let sy = y + m.ch * 0.52;
            scene.line(cx, sy, cx + sw, sy, 1.0, theme::INK_4, clip);
        }
        MdStyle::Link => {
            let link_col = theme::rgb(0x79, 0xb8, 0xff);
            scene.text_with_face(cx, y, txt, link_col, force_bold, false, prose_face, clip);
            let uy = y + m.ch - 1.5;
            scene.line(cx, uy, cx + sw, uy, 1.0, theme::rgba(0x79, 0xb8, 0xff, 0.38), clip);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_code_lines(
    scene: &mut Scene,
    lines: &[CodeLine<'_>],
    x0: f32,
    mut y: f32,
    w: f32,
    max_y: f32,
    m: CellMetrics,
    clip: u32,
    show_gutter: bool,
) -> f32 {
    let line_step = m.ch + 2.0;
    let gutter_w = if show_gutter { 4.2 * m.cw } else { 0.0 };
    let max_cols = ((w - gutter_w) / m.cw).floor().max(8.0) as usize;
    for line in lines {
        if y + m.ch > max_y {
            break;
        }
        if show_gutter {
            scene.text(x0, y, format!("{:>3}", line.line_no), theme::INK_4, false, false, clip);
        }
        let mut cx = x0 + gutter_w;
        let mut rem = max_cols;
        for sp in line.spans.iter() {
            if rem == 0 {
                break;
            }
            let clean = sp.text.as_str().replace('\t', "    ");
            let txt = truncate_chars(&clean, rem);
            let len = txt.chars().count();
            let (col, bold, italic) = token_color(sp.kind);
            scene.text(cx, y, &txt, col, bold, italic, clip);
            cx += len as f32 * m.cw;
            rem = rem.saturating_sub(len);
        }
        y += line_step;
    }
    y
}
