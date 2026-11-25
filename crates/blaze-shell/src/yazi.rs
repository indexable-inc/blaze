//! Native Yazi-class 3-column GPU file manager (`y` / `y <dir>`):
//! Miller columns (parent | current | live preview) rendered directly in Metal with macOS blue
//! folders, IntelliJ New UI dark SVG icons, instant text/dir previews, and Vim navigation
//! (`j`/`k`/`h`/`l`/`gg`/`G`/`/` filter/`Enter`/`Space` insert/`q`/`Esc`).

use std::fs;
use std::path::{Path, PathBuf};

use crate::blocks::BlockStore;
use crate::layout::{TOP_BAR_H, layout_top_bar};
use crate::prompt::PromptInfo;
use crate::svg_icons::icon_name_for_file;
use crate::theme;
use crate::view::{CellMetrics, Scene, truncate_chars};

fn format_size_bytes(bytes: i64) -> String {
    let mut s = String::new();
    let _ = blaze_sh::Value::Size(blaze_sh::Bytes(bytes)).format_cell(&mut s);
    s
}

const MAX_PREVIEW_BYTES: usize = 8192;
const MAX_PREVIEW_LINES: usize = 64;
const MAX_DIR_ENTRIES: usize = 512;

/// One entry inside a Miller column.
#[derive(Clone, Debug)]
pub struct YaziEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub size: u64,
    pub icon: &'static str,
}

/// Preview content for the currently highlighted entry in the right pane.
#[derive(Clone, Debug)]
pub enum YaziPreview {
    Empty,
    Directory(Vec<YaziEntry>),
    Text {
        meta: String,
        lines: Vec<String>,
        truncated: bool,
    },
    Binary {
        meta: String,
    },
    Error(String),
}

/// Outcome returned by [`YaziState::handle_key`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum YaziAction {
    Continue,
    /// Exit `y` and `cd` into the directory the user navigated to.
    ExitAndCd(PathBuf),
    /// Exit `y` without changing directory.
    ExitCancel,
    /// Exit `y` and insert the selected path into the shell prompt.
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
            filter: String::new(),
            filtering: false,
            show_hidden: false,
            pending_g: false,
            preview: YaziPreview::Empty,
        };
        st.reload(None);
        st
    }

    /// Return filtered indices into `self.entries`.
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
        let Some(entry) = self.current_entry().cloned() else {
            self.preview = YaziPreview::Empty;
            return;
        };
        if entry.is_dir {
            let sub = read_dir_entries(&entry.path, self.show_hidden);
            self.preview = YaziPreview::Directory(sub);
            return;
        }
        self.preview = load_file_preview_body(&entry.path, entry.size);
    }

    /// Handle one keyboard input in `y`.
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

/// Read a bounded, clean file preview for both `y` and `ls` hover popovers!
pub fn load_file_preview_lines(path: &Path, max_lines: usize) -> (String, Vec<String>, bool) {
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) => return ("unreadable".to_owned(), vec![format!("{e}")], false),
    };
    if meta.is_dir() {
        let entries = read_dir_entries(path, false);
        let total = entries.len();
        let lines: Vec<String> = entries
            .into_iter()
            .take(max_lines)
            .map(|e| if e.is_dir { format!("{}/", e.name) } else { e.name })
            .collect();
        let meta_str = format!("dir · {total} items");
        return (meta_str, lines, total > max_lines);
    }
    let size = meta.len();
    match load_file_preview_body(path, size) {
        YaziPreview::Text { meta, mut lines, mut truncated } => {
            if lines.len() > max_lines {
                lines.truncate(max_lines);
                truncated = true;
            }
            (meta, lines, truncated)
        }
        YaziPreview::Binary { meta } => (meta, vec!["<binary file>".to_owned()], false),
        YaziPreview::Error(e) => ("error".to_owned(), vec![e], false),
        _ => (format_size_bytes(size as i64), vec!["(empty file)".to_owned()], false),
    }
}

fn load_file_preview_body(path: &Path, size: u64) -> YaziPreview {
    use std::io::Read as _;
    let Ok(mut f) = fs::File::open(path) else {
        return YaziPreview::Error("permission denied".to_owned());
    };
    let mut buf = vec![0u8; (size as usize).min(MAX_PREVIEW_BYTES)];
    let n = f.read(&mut buf).unwrap_or(0);
    buf.truncate(n);
    if buf.iter().take(512).any(|&b| b == 0) {
        return YaziPreview::Binary {
            meta: format!("binary · {}", format_size_bytes(size as i64)),
        };
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines = Vec::new();
    let mut truncated = (size as usize) > MAX_PREVIEW_BYTES;
    for line in text.lines() {
        if lines.len() >= MAX_PREVIEW_LINES {
            truncated = true;
            break;
        }
        lines.push(line.replace('\t', "    "));
    }
    if lines.is_empty() {
        lines.push("(empty file)".to_owned());
    }
    YaziPreview::Text {
        meta: format_size_bytes(size as i64),
        lines,
        truncated,
    }
}

fn read_dir_entries(dir: &Path, show_hidden: bool) -> Vec<YaziEntry> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        if out.len() >= MAX_DIR_ENTRIES {
            break;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !show_hidden && name.starts_with('.') {
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
    out
}

/// Lay out the native 3-column Miller file browser (`y`) directly in Metal, keeping the
/// unified top tab bar visible and clickable at the top of the window.
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
    let mut scene = Scene::new();
    let vw = viewport_w.max(400.0);
    let vh = viewport_h.max(260.0);

    let header_h = m.ch + 14.0;
    let footer_h = m.ch + 12.0;
    let body_top = TOP_BAR_H + header_h;
    let body_bot = (vh - footer_h).max(body_top + 40.0);
    let body_h = body_bot - body_top;

    // 1. Path breadcrumb header bar directly below the titlebar.
    scene.round_rect(0.0, TOP_BAR_H, vw, body_top, 0.0, theme::BG_SURFACE, 0.0, [0.0; 4], 0);
    scene.line(0.0, body_top - 0.5, vw, body_top - 0.5, 1.0, theme::LINE, 0);
    let hty = TOP_BAR_H + (header_h - m.ch) * 0.5;
    scene.svg_icon(14.0, hty + (m.ch - 13.0) * 0.5, 13.0, "folder_macos", 0);
    let cwd_str = yazi.cwd.to_string_lossy();
    let max_path_c = ((vw - 260.0) / m.cw).floor().max(16.0) as usize;
    scene.text(34.0, hty, truncate_chars(&cwd_str, max_path_c), theme::INK_1, true, false, 0);

    if yazi.filtering || !yazi.filter.is_empty() {
        let flbl = format!("/{}", yazi.filter);
        let fw = flbl.chars().count() as f32 * m.cw + 14.0;
        let fx0 = vw - 14.0 - fw;
        scene.round_rect(fx0, TOP_BAR_H + 4.0, vw - 14.0, body_top - 4.0, 5.0, theme::BG_CHIP, 1.0, theme::LINE_STRONG, 0);
        scene.text(fx0 + 7.0, hty, flbl, theme::INK_1, false, false, 0);
    }

    // 2. Three Miller columns: Parent (18%) | Current (34%) | Live Preview (48%).
    let col1_x0 = 0.0;
    let col1_x1 = (vw * 0.18).clamp(130.0, 240.0);
    let col2_x0 = col1_x1;
    let col2_x1 = (col2_x0 + vw * 0.34).clamp(col2_x0 + 200.0, vw - 220.0);
    let col3_x0 = col2_x1;
    let col3_x1 = vw;

    scene.line(col1_x1, body_top, col1_x1, body_bot, 1.0, theme::LINE_FAINT, 0);
    scene.line(col2_x1, body_top, col2_x1, body_bot, 1.0, theme::LINE_FAINT, 0);

    let row_h = m.ch + 6.0;
    let max_vis_rows = ((body_h - 12.0) / row_h).floor().max(1.0) as usize;
    let icon_s = 13.0;

    // Column 1: Parent directory (muted context).
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
                scene.round_rect(
                    col2_x0 + 6.0,
                    ry,
                    col2_x1 - 6.0,
                    ry + row_h,
                    5.0,
                    theme::rgb(0x1e, 0x1e, 0x22),
                    0.0,
                    [0.0; 4],
                    clip2,
                );
                scene.round_rect(
                    col2_x0 + 6.0,
                    ry + 4.0,
                    col2_x0 + 8.0,
                    ry + row_h - 4.0,
                    1.0,
                    theme::INK_1,
                    0.0,
                    [0.0; 4],
                    clip2,
                );
            }
            let ty = ry + (row_h - m.ch) * 0.5;
            scene.svg_icon(col2_x0 + 14.0, ty + (m.ch - icon_s) * 0.5, icon_s, entry.icon, clip2);
            let sz_lbl = if entry.is_dir {
                String::new()
            } else {
                format_size_bytes(entry.size as i64)
            };
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

    // Column 3: Live content / directory preview pane.
    let clip3 = scene.push_clip(col3_x0 + 1.0, body_top, col3_x1, body_bot, 0.0);
    let px = col3_x0 + 16.0;
    let pw = (col3_x1 - px - 14.0).max(80.0);
    match &yazi.preview {
        YaziPreview::Empty => {
            scene.text(px, body_top + 12.0, "no selection", theme::INK_4, false, true, clip3);
        }
        YaziPreview::Directory(items) => {
            if items.is_empty() {
                scene.text(px, body_top + 12.0, "empty directory", theme::INK_4, false, true, clip3);
            } else {
                for (i, sub) in items.iter().take(max_vis_rows).enumerate() {
                    let ry = body_top + 6.0 + i as f32 * row_h;
                    let ty = ry + (row_h - m.ch) * 0.5;
                    scene.svg_icon(px, ty + (m.ch - icon_s) * 0.5, icon_s, sub.icon, clip3);
                    let max_c = ((pw - 24.0) / m.cw).floor().max(8.0) as usize;
                    let lbl = if sub.is_dir { format!("{}/", sub.name) } else { sub.name.clone() };
                    scene.text(px + 20.0, ty, truncate_chars(&lbl, max_c), theme::INK_2, false, false, clip3);
                }
            }
        }
        YaziPreview::Text { lines, truncated, .. } => {
            let line_step = m.ch + 2.0;
            let gutter_w = 4.0 * m.cw;
            let max_c = ((pw - gutter_w) / m.cw).floor().max(12.0) as usize;
            let max_lines = ((body_h - 16.0) / line_step).floor().max(1.0) as usize;
            for (i, line) in lines.iter().take(max_lines).enumerate() {
                let ly = body_top + 8.0 + i as f32 * line_step;
                scene.text(px, ly, format!("{:>3}", i + 1), theme::INK_4, false, false, clip3);
                let col = if line.trim_start().starts_with("//") || line.trim_start().starts_with('#') {
                    theme::INK_3
                } else {
                    theme::INK_2
                };
                scene.text(px + gutter_w, ly, truncate_chars(line, max_c), col, false, false, clip3);
            }
            if *truncated && lines.len() < max_lines {
                let ly = body_top + 8.0 + lines.len() as f32 * line_step;
                scene.text(px + gutter_w, ly, "… truncated", theme::INK_4, false, true, clip3);
            }
        }
        YaziPreview::Binary { meta } => {
            scene.text(px, body_top + 14.0, meta, theme::INK_3, false, false, clip3);
            scene.text(px, body_top + 14.0 + m.ch + 6.0, "binary preview skipped", theme::INK_4, false, true, clip3);
        }
        YaziPreview::Error(err) => {
            scene.text(px, body_top + 14.0, err, theme::DANGER, false, false, clip3);
        }
    }

    // 3. Minimal status footer bar.
    scene.round_rect(0.0, body_bot, vw, vh, 0.0, theme::BG_SURFACE, 0.0, [0.0; 4], 0);
    scene.line(0.0, body_bot, vw, body_bot, 1.0, theme::LINE, 0);
    let fty = body_bot + (footer_h - m.ch) * 0.5;
    let pos_lbl = if vis.is_empty() {
        "y · 0/0".to_owned()
    } else {
        format!("y · {}/{}", yazi.selected + 1, vis.len())
    };
    scene.text(14.0, fty, pos_lbl, theme::INK_2, true, false, 0);
    let hint = "hjkl move  ·  / filter  ·  . hidden  ·  space insert  ·  q cd & exit";
    let hw = hint.chars().count() as f32 * m.cw;
    if vw - 14.0 - hw > 140.0 {
        scene.text(vw - 14.0 - hw, fty, hint, theme::INK_4, false, false, 0);
    }

    // 4. Unified top tab bar always visible!
    layout_top_bar(&mut scene, store, prompt, vw, m, zoom_pct, inline_titlebar);
    scene
}
