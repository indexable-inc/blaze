//! GPU-independent visual scene primitives, hit-test regions, and structured value
//! formatting helpers consumed by `layout.rs` and rasterized by Metal (`render.rs`).

use blaze_sh::Value;
use blaze_ui::shaders::{ClipRectGpu, RectInst, prim_kind};

use crate::blocks::{BlockId, BranchId};
use crate::theme;
use crate::tiling::{PaneId, SplitAxis, TabId};

/// Typography face & scale role: dual-font SF Pro (UI chrome & prose) + SF Mono (code & tabular data).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TextFace {
    /// Primary monospace (`1.0×` SF Mono / Berkeley Mono / JetBrains Mono): commands, code, paths, PTY, tabular numbers.
    #[default]
    Mono,
    /// Compact monospace (`0.84×` of the primary mono family): scrollback table headers, status labels, exit/meta pills.
    MonoSmall,
    /// Proportional UI sans (`0.94×` Apple SF Pro): prose, overlays, command palette — scales with zoom.
    Ui,
    /// Compact proportional UI sans (`0.84×` Apple SF Pro): overlay hints, popover docs, sidebar metadata — scales with zoom.
    UiSmall,
    /// Scaled proportional heading (`1.18×` Apple SF Pro Semibold): rendered Markdown headings — scales with zoom.
    Heading,
    /// Fixed native macOS titlebar & pane-header chrome (`12pt` SF Pro, immune to `Cmd+`/`Cmd-` zoom so fixed titlebars never clip).
    Chrome,
}

/// Logical CSS-pixel dimensions of one monospace cell, with exact CoreText ASCII advance widths
/// for Apple SF Pro proportional faces so inline spans (`<a>`, `<code>`, `<b>`) never overlap.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellMetrics {
    pub cw: f32,
    pub ch: f32,
    /// Exact logical-pixel CoreText advances for ASCII `32..=126` in `TextFace::Ui` Regular (`0.0` = heuristic fallback).
    pub ui_adv: [f32; 95],
    /// Exact logical-pixel CoreText advances for ASCII `32..=126` in `TextFace::Ui` Bold/Semibold (`0.0` = heuristic fallback).
    pub ui_bold_adv: [f32; 95],
    /// Exact logical-pixel advance ratio `Chrome / Ui`.
    pub chrome_ratio: f32,
}

impl Default for CellMetrics {
    fn default() -> Self {
        Self {
            cw: 8.0,
            ch: 18.0,
            ui_adv: [0.0; 95],
            ui_bold_adv: [0.0; 95],
            chrome_ratio: 0.92,
        }
    }
}

impl CellMetrics {
    /// Standard content SVG icon size (folders, files, MacBook Pro, toolchains) scaled to text height.
    #[inline]
    pub fn icon_md(&self) -> f32 {
        (self.ch * 0.78).round().max(11.0)
    }

    /// Compact inline vector icon size (git branch, stash, lock, block buttons) scaled to text height.
    #[inline]
    pub fn icon_sm(&self) -> f32 {
        (self.ch * 0.68).round().max(10.0)
    }

    /// Exact logical width of `text` in `face` (`len * cw` for `Mono`; calibrated CoreText advances for SF Pro).
    pub fn measure(&self, text: &str, face: TextFace) -> f32 {
        self.measure_styled(text, face, false)
    }

    /// Exact logical width of `text` in `face` with exact CoreText regular vs bold glyph advances.
    pub fn measure_styled(&self, text: &str, face: TextFace, bold: bool) -> f32 {
        if face == TextFace::Mono {
            return text.chars().count() as f32 * self.cw;
        }
        if face == TextFace::MonoSmall {
            return text.chars().count() as f32 * (self.cw * 0.84);
        }
        // Use exact measured CoreText glyph advances when populated (`Fonts::cell_metrics`).
        let use_bold_table = (bold || face == TextFace::Heading) && self.ui_bold_adv[b'n' as usize - 32] > 0.0;
        let table = if use_bold_table { &self.ui_bold_adv } else { &self.ui_adv };
        if table[b'n' as usize - 32] > 0.0 {
            let ratio = match face {
                TextFace::Mono | TextFace::MonoSmall => 1.0,
                TextFace::Ui => 1.0,
                TextFace::UiSmall => 0.84 / 0.94,
                TextFace::Heading => 1.18 / 0.94,
                TextFace::Chrome => self.chrome_ratio,
            };
            let fallback_non_ascii = table[b'm' as usize - 32].max(self.cw * 0.85);
            let mut sum = 0.0f32;
            for ch in text.chars() {
                let code = ch as u32;
                if (32..=126).contains(&code) {
                    sum += table[(code - 32) as usize];
                } else if ch == '·' {
                    sum += table[b'.' as usize - 32] * 1.05;
                } else if ch == '—' || ch == '…' {
                    sum += table[b'M' as usize - 32] * 1.1;
                } else {
                    sum += fallback_non_ascii;
                }
            }
            return sum * ratio;
        }

        // Heuristic fallback for headless unit tests without CoreText.
        let bold_mul = if bold && face != TextFace::Heading { 1.075 } else { 1.0 };
        let base_cw = if face == TextFace::Chrome { 7.8 } else { self.cw };
        let scale = match face {
            TextFace::Mono => 1.0,
            TextFace::MonoSmall => 0.84,
            TextFace::Ui => 0.92,
            TextFace::UiSmall => 0.82,
            TextFace::Heading => 1.15,
            TextFace::Chrome => 0.86,
        };
        let mut units = 0.0f32;
        for ch in text.chars() {
            units += match ch {
                ' ' => 0.42,
                'i' | 'l' | 'I' | '!' | '|' | '.' | ',' | ':' | ';' | '\'' | '`' => 0.44,
                'f' | 'j' | 'r' | 't' | '(' | ')' | '[' | ']' | '/' | '\\' | '-' | '·' => 0.58,
                'm' | 'w' | 'M' | 'W' | '…' | '—' => 1.24,
                c if c.is_ascii_uppercase() => 1.04,
                c if !c.is_ascii() => 1.15,
                _ => 0.88,
            };
        }
        units * base_cw * scale * bold_mul
    }

    /// Logical line height for `face`.
    #[inline]
    pub fn line_h(&self, face: TextFace) -> f32 {
        match face {
            TextFace::Mono => self.ch,
            TextFace::MonoSmall => self.ch * 0.88,
            TextFace::Ui => self.ch * 0.96,
            TextFace::UiSmall => self.ch * 0.88,
            TextFace::Heading => self.ch * 1.22,
            TextFace::Chrome => 16.0,
        }
    }
}

/// A styled text span positioned in logical pixels to be turned into [`blaze_ui::shaders::GlyphInst`]s.
#[derive(Clone, Debug, PartialEq)]
pub struct TextRun {
    pub x: f32,
    pub y: f32,
    pub text: String,
    pub color: [f32; 4],
    pub bold: bool,
    pub italic: bool,
    pub face: TextFace,
    pub clip_id: u32,
}

/// Axis-aligned clickable hit box in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HitRect {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl HitRect {
    #[inline]
    pub const fn new(x0: f32, y0: f32, x1: f32, y1: f32) -> Self {
        Self { x0, y0, x1, y1 }
    }

    #[inline]
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x0 && x <= self.x1 && y >= self.y0 && y <= self.y1
    }
}

impl From<blaze_tiling::TileRect> for HitRect {
    #[inline]
    fn from(r: blaze_tiling::TileRect) -> Self {
        Self {
            x0: r.x0,
            y0: r.y0,
            x1: r.x1,
            y1: r.y1,
        }
    }
}

impl From<HitRect> for blaze_tiling::TileRect {
    #[inline]
    fn from(r: HitRect) -> Self {
        Self {
            x0: r.x0,
            y0: r.y0,
            x1: r.x1,
            y1: r.y1,
        }
    }
}

/// A Retina SVG icon quad positioned in logical pixels, resolved through `IconAtlas`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IconQuad {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub name: &'static str,
    pub alpha: f32,
    pub clip_id: u32,
}

/// A full-color Retina Apple Color Emoji / custom Unicode symbol quad resolved through `IconAtlas`.
#[derive(Clone, Debug, PartialEq)]
pub struct EmojiQuad {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub text: String,
    pub alpha: f32,
    pub clip_id: u32,
}

/// A Retina custom symbol registered via the Blaze Terminal API (SVG, in-memory font glyph,
/// PNG bitmap, or composite text), positioned in logical pixels and rasterized through `IconAtlas`.
#[derive(Clone, Debug, PartialEq)]
pub struct CustomSymbolQuad {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub name: String,
    /// Optional RGBA tint applied on the GPU (used for monochrome SVGs and untinted font glyphs).
    pub tint: Option<[f32; 4]>,
    pub alpha: f32,
    pub clip_id: u32,
}

/// Logical display width for a custom emoji / symbol badge of height `h`.
#[inline]
pub fn symbol_badge_width(text: &str, h: f32) -> f32 {
    let clusters = crate::config::symbol_cluster_count(text).clamp(1, 6) as f32;
    (clusters * h * 1.12).max(h)
}

/// A decoded raster image quad (`webp`, `png`, `jpg`, `gif`) positioned in logical pixels,
/// resolved through `ImageAtlas` for rendered Markdown & HTML `<picture>` / `<img>` elements.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageQuad {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub path: String,
    pub alpha: f32,
    pub circle: bool,
    pub clip_id: u32,
}

/// Semantic UI action triggered by clicking a [`HitRegion`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HitAction {
    SwitchBranch(BranchId),
    ForkActiveBranch,
    ForkBlock(BlockId),
    ToggleFold(BlockId),
    ToggleTableView(BlockId),
    ToggleHiddenCols(BlockId),
    TogglePtyFullscreen,
    FocusInlinePty(BlockId),
    FindToggleCase,
    FindNext,
    FindPrev,
    FindClose,
    SysMonToggleTree(BlockId),
    SysMonToggleCumulative(BlockId),
    SysMonRequestKill(BlockId),
    SysMonTogglePause(BlockId),
    SysMonSort { block: BlockId, col: u8 },
    SysMonSelectRow { block: BlockId, row: usize },
    SysMonToggleCollapse { block: BlockId, pid: u32 },
    SysMonClose(BlockId),
    /// Switch to the workspace tab & pane owning a running process/TUI and scroll to its block.
    FocusPaneBlock { pane: crate::tiling::PaneId, block: BlockId },
    /// Open native Activity & GPU Monitor (`top`) focused/filtered on a specific OS PID.
    InspectProcessPid(u32),
    /// Terminate a specific OS PID directly from the folder-processes popover (SIGTERM; Shift+click SIGKILL).
    KillProcessPid(u32),
    SortTableColumn { block: BlockId, col: usize },
    ResizeTableColumn { block: BlockId, col: usize, col_x0: i32 },
    ToggleExpandCell { block: BlockId, row: usize, col: usize },
    SelectCompletion(usize),
    SelectHistory(usize),
    SelectPalette(usize),
    CycleHistoryScope,
    ResetZoom,
    /// Interactive prompt-capsule popover actions.
    OpenBranchPicker,
    SwitchGitBranch(String),
    ChangeDir(String),
    InsertToken(String),
    /// Replace `[replace_start..cursor]` in the active editor with `insert` (from the zero-Tab live hint strip).
    InsertLiveHint { replace_start: usize, insert: String },
    /// Submit a command directly into the focused pane (e.g. clicking `git help tutorial ↗` or `?` in help cards).
    RunCommand(String),
    /// In-app tiling window manager & workspace tab actions.
    SwitchTab(TabId),
    /// Switch to workspace `tab` and activate CoW session fork `branch` in its focused pane.
    SwitchTabBranch { tab: TabId, branch: BranchId },
    /// Toggle the collapsible left workspace & fork-hierarchy sidebar (`Cmd+S`).
    ToggleSidebar,
    NewTab,
    CloseTab(TabId),
    SplitPane(SplitAxis),
    NewWindow,
    FocusPane(PaneId),
    ClosePane(PaneId),
    ToggleZoomPane(PaneId),
    /// Rendered Markdown/HTML interactive actions.
    OpenLink { href: String, base_dir: String },
    CopyText(String),
    /// Agent Hub interactive actions (resizable sidebar divider, clickable rows, toolbar & kill).
    ResizeAgentSidebar,
    SelectAgent(usize),
    AgentToggleOffline,
    AgentReprobeServers,
    AgentAttachSelected,
    AgentNewTaskSelected,
    AgentRequestKill,
    AgentConfirmKill { sigkill: bool },
    AgentCancelKill,
}

/// Clickable UI target recorded during layout.
#[derive(Clone, Debug, PartialEq)]
pub struct HitRegion {
    pub rect: HitRect,
    pub action: HitAction,
}

/// One styled span inside a floating file preview popover (produced by `blaze-preview`).
#[derive(Clone, Debug, PartialEq)]
pub struct PreviewSpan {
    pub text: String,
    pub color: [f32; 4],
    pub bold: bool,
    pub italic: bool,
}

/// One line inside a floating file preview popover (with optional 1-based line number gutter).
#[derive(Clone, Debug, PartialEq)]
pub struct PreviewLine {
    pub line_no: Option<u32>,
    pub spans: Vec<PreviewSpan>,
}

/// One entry inside a rich directory hover preview popover.
#[derive(Clone, Debug, PartialEq)]
pub struct DirPreviewEntry {
    pub name: String,
    pub icon: &'static str,
    pub is_dir: bool,
    pub size_label: String,
    pub kind_label: &'static str,
    pub muted: bool,
}

/// Rich interactive hover popover kind attached to prompt capsule segments, icon buttons, & files.
#[derive(Clone, Debug, PartialEq)]
pub enum HoverKind {
    Tooltip { title: String, subtitle: Option<String> },
    Directory { cwd: String },
    GitBranch {
        current: String,
        branches: Vec<String>,
        relation: Option<crate::prompt::BranchRelation>,
    },
    GitChanges {
        files: Vec<(String, &'static str, &'static str)>,
        details: Vec<crate::prompt::ChangedFileDetail>,
        total: u32,
    },
    GitStashes {
        stashes: Vec<String>,
        entries: Vec<crate::prompt::GitStashEntry>,
        total: u32,
    },
    Toolchains { items: Vec<(&'static str, &'static str, &'static str)> },
    /// Hovering a file entry in `ls` grid or table; lazily hydrated into `FilePreview` / `DirPreview`.
    FileEntry { full_path: String, name: String, icon: Option<&'static str>, is_dir: bool },
    /// Hydrated rich syntax-highlighted / tabular / markdown preview popover rendered in dark glass.
    FilePreview {
        full_path: String,
        name: String,
        icon: Option<&'static str>,
        meta: String,
        lines: Vec<PreviewLine>,
        truncated: bool,
    },
    /// Rich directory hover preview with SVG icons, `blaze-sniff` analysis, and right-aligned sizes.
    DirPreview {
        full_path: String,
        name: String,
        icon: Option<&'static str>,
        meta: String,
        entries: Vec<DirPreviewEntry>,
        total_items: usize,
        hidden_count: usize,
    },
    /// Live processes running inside `cwd` (and subfolders + grandchildren/great-grandchildren).
    FolderProcs {
        summary: crate::sysmon_driver::FolderProcSnapshot,
    },
    /// Hoverable token span inside a command line; lazily hydrated into `CommandExplain`.
    CommandToken {
        source: String,
        token_start: usize,
        token_end: usize,
    },
    /// Hydrated explanation popover for a hovered command/subcommand/flag/operator token.
    CommandExplain {
        source: String,
        token_start: usize,
        token_end: usize,
        badge: &'static str,
        title: String,
        summary: String,
        details: Vec<(String, String)>,
        source_note: String,
    },
}

/// Hoverable region recorded during layout.
#[derive(Clone, Debug, PartialEq)]
pub struct HoverRegion {
    pub rect: HitRect,
    pub kind: HoverKind,
}

/// Active floating popover anchored to a prompt segment or icon button.
#[derive(Clone, Debug, PartialEq)]
pub struct ActivePopover {
    pub anchor: HitRect,
    pub kind: HoverKind,
    pub pinned: bool,
}

/// Scrollable embedded table viewport recorded during layout for nested trackpad scrolling.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollRegion {
    pub rect: HitRect,
    pub block: BlockId,
    pub max_scroll_y: f32,
    pub max_scroll_x: f32,
}

/// Payload carried when dragging a rendered item (file, folder, crate, value) into the composer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DragPayload {
    pub label: String,
    pub insert_text: String,
    pub icon: Option<&'static str>,
}

/// Draggable region recorded during layout.
#[derive(Clone, Debug, PartialEq)]
pub struct DragSource {
    pub rect: HitRect,
    pub payload: DragPayload,
}

/// Live in-flight drag state passed into layout so the floating drag pill and composer drop glow render.
#[derive(Clone, Debug, PartialEq)]
pub struct ActiveDrag {
    pub x: f32,
    pub y: f32,
    pub over_composer: bool,
    pub payload: DragPayload,
}

/// Active Ghostty-style cursor sweep trail (`DURATION = 0.2s`, `TRAIL_LENGTH = 0.5`, `EaseOutCubic`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CursorTrail {
    pub prev_rect: [f32; 4],
    pub progress: f32,
}

/// Active mouse drag-selection across rendered output/terminal text.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextSelection {
    pub anchor: [f32; 2],
    pub head: [f32; 2],
}

/// Visual selection granularity during Vim scrollback/output browsing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputVisualMode {
    Char,
    Line,
}

/// Vim Normal/Visual cursor browsing previous command outputs (`h`/`j`/`k`/`l`, `v`/`V`, `y`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputNav {
    /// Index into extracted visible visual lines (`0` = topmost visible line below titlebar).
    pub line: usize,
    /// Character column within `line`.
    pub col: usize,
    /// Optional visual anchor `(line, col)` and mode when `v` or `V` is active.
    pub visual: Option<(usize, usize, OutputVisualMode)>,
    /// True after pressing `g` once (for `gg` jump to top).
    pub pending_g: bool,
}

/// One visual row of rendered text extracted from [`Scene::text_runs`] for Vim output navigation.
#[derive(Clone, Debug)]
pub struct VisualLine {
    pub y: f32,
    pub chars: Vec<(f32, char)>,
}

/// Extract ordered visual rows from `runs` (ignoring titlebar runs above `min_y`).
pub fn extract_visual_lines(runs: &[TextRun], m: CellMetrics, min_y: f32, max_y: f32) -> Vec<VisualLine> {
    if m.cw <= 0.0 || m.ch <= 0.0 {
        return Vec::new();
    }
    let mut eligible: Vec<&TextRun> = runs
        .iter()
        .filter(|r| !r.text.is_empty() && r.y >= min_y && r.y + m.ch <= max_y)
        .collect();
    if eligible.is_empty() {
        return Vec::new();
    }
    eligible.sort_by(|a, b| {
        a.y.partial_cmp(&b.y)
            .unwrap_or(core::cmp::Ordering::Equal)
            .then_with(|| a.x.partial_cmp(&b.x).unwrap_or(core::cmp::Ordering::Equal))
    });
    let mut grouped: Vec<(f32, Vec<&TextRun>)> = Vec::new();
    for run in eligible {
        if let Some((ly, row)) = grouped.last_mut()
            && (run.y - *ly).abs() <= m.ch * 0.45
        {
            row.push(run);
        } else {
            grouped.push((run.y, vec![run]));
        }
    }
    let mut out = Vec::with_capacity(grouped.len());
    for (ly, mut row) in grouped {
        row.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(core::cmp::Ordering::Equal));
        let mut chars: Vec<(f32, char)> = Vec::new();
        let mut prev_end_x: Option<f32> = None;
        for run in row {
            if let Some(pe) = prev_end_x
                && run.x - pe >= m.cw * 0.65
            {
                chars.push((pe, ' '));
            }
            for (ci, ch) in run.text.chars().enumerate() {
                chars.push((run.x + ci as f32 * m.cw, ch));
            }
            prev_end_x = Some(run.x + run.text.chars().count() as f32 * m.cw);
        }
        if !chars.is_empty() {
            out.push(VisualLine { y: ly, chars });
        }
    }
    out
}

/// Compute cursor rect, visual selection highlight rects, and yanked text for [`OutputNav`].
pub fn output_nav_geometry(
    lines: &[VisualLine],
    m: CellMetrics,
    nav: OutputNav,
) -> (Option<[f32; 4]>, Vec<[f32; 4]>, String) {
    if lines.is_empty() {
        return (None, Vec::new(), String::new());
    }
    let li = nav.line.min(lines.len() - 1);
    let row = &lines[li];
    let ci = nav.col.min(row.chars.len().saturating_sub(1));
    let cx = row.chars.get(ci).map(|(x, _)| *x).unwrap_or(0.0);
    let cursor_rect = Some([cx, row.y, cx + m.cw, row.y + m.ch]);

    let Some((al, ac, mode)) = nav.visual else {
        let ch_str = row.chars.get(ci).map(|(_, c)| c.to_string()).unwrap_or_default();
        return (cursor_rect, Vec::new(), ch_str);
    };
    let al = al.min(lines.len() - 1);
    let ((l0, c0), (l1, c1)) = if (al, ac) <= (li, ci) {
        ((al, ac), (li, ci))
    } else {
        ((li, ci), (al, ac))
    };

    let mut rects = Vec::new();
    let mut text_lines = Vec::new();
    for (idx, line) in lines.iter().enumerate().take(l1 + 1).skip(l0) {
        if line.chars.is_empty() {
            continue;
        }
        let (sc, ec) = match mode {
            OutputVisualMode::Line => (0, line.chars.len() - 1),
            OutputVisualMode::Char => {
                let s = if idx == l0 { c0.min(line.chars.len() - 1) } else { 0 };
                let e = if idx == l1 { c1.min(line.chars.len() - 1) } else { line.chars.len() - 1 };
                (s.min(e), e.max(s))
            }
        };
        let x0 = line.chars[sc].0;
        let x1 = line.chars[ec].0 + m.cw;
        rects.push([x0, line.y, x1, line.y + m.ch]);
        let s: String = line.chars[sc..=ec].iter().map(|(_, c)| *c).collect();
        text_lines.push(s);
    }
    (cursor_rect, rects, text_lines.join("\n"))
}

/// Compute highlight rectangles and extracted plain text for `sel` over `runs` (all in logical
/// screen coordinates). Runs above `min_y` (such as the top titlebar) are ignored.
pub fn compute_text_selection(
    runs: &[TextRun],
    m: CellMetrics,
    sel: TextSelection,
    min_y: f32,
) -> (Vec<[f32; 4]>, String) {
    if m.cw <= 0.0 || m.ch <= 0.0 {
        return (Vec::new(), String::new());
    }
    let mut eligible: Vec<&TextRun> = runs
        .iter()
        .filter(|r| !r.text.is_empty() && r.y + m.ch > min_y)
        .collect();
    if eligible.is_empty() {
        return (Vec::new(), String::new());
    }
    eligible.sort_by(|a, b| {
        a.y.partial_cmp(&b.y)
            .unwrap_or(core::cmp::Ordering::Equal)
            .then_with(|| a.x.partial_cmp(&b.x).unwrap_or(core::cmp::Ordering::Equal))
    });

    // Group runs into visual lines by vertical proximity.
    let mut lines: Vec<(f32, Vec<&TextRun>)> = Vec::new();
    for run in eligible {
        if let Some((ly, row)) = lines.last_mut()
            && (run.y - *ly).abs() <= m.ch * 0.45
        {
            row.push(run);
        } else {
            lines.push((run.y, vec![run]));
        }
    }
    for (_, row) in &mut lines {
        row.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(core::cmp::Ordering::Equal));
    }

    // Normalize anchor/head into top-to-bottom, left-to-right order.
    let same_row = (sel.anchor[1] - sel.head[1]).abs() <= m.ch * 0.55;
    let (start, end) = if same_row {
        if sel.anchor[0] <= sel.head[0] {
            (sel.anchor, sel.head)
        } else {
            (sel.head, sel.anchor)
        }
    } else if sel.anchor[1] < sel.head[1] {
        (sel.anchor, sel.head)
    } else {
        (sel.head, sel.anchor)
    };

    // Find visual line indices intersecting [start_y, end_y].
    let line_for_y = |py: f32| -> Option<usize> {
        lines.iter().position(|(ly, _)| py >= *ly - 2.0 && py <= *ly + m.ch + 2.0)
    };
    let first_line = line_for_y(start[1])
        .or_else(|| lines.iter().position(|(ly, _)| *ly + m.ch >= start[1]));
    let last_line = line_for_y(end[1])
        .or_else(|| lines.iter().rposition(|(ly, _)| *ly <= end[1]));
    let (Some(l0), Some(l1)) = (first_line, last_line) else {
        return (Vec::new(), String::new());
    };
    if l0 > l1 {
        return (Vec::new(), String::new());
    }

    let mut rects = Vec::new();
    let mut out_lines: Vec<String> = Vec::new();

    for (li, (ly, row)) in lines.iter().enumerate().take(l1 + 1).skip(l0) {
        let row_x0 = if li == l0 { start[0] } else { f32::NEG_INFINITY };
        let row_x1 = if li == l1 { end[0] } else { f32::INFINITY };

        let mut line_txt = String::new();
        let mut prev_end_x: Option<f32> = None;
        let mut rect_x0: Option<f32> = None;
        let mut rect_x1: f32 = 0.0;

        for run in row {
            let nchars = run.text.chars().count();
            let run_x0 = run.x;
            let run_x1 = run.x + nchars as f32 * m.cw;
            if run_x1 <= row_x0 || run_x0 >= row_x1 {
                continue;
            }
            let c0 = if row_x0 <= run_x0 {
                0
            } else {
                (((row_x0 - run_x0) / m.cw).floor().max(0.0) as usize).min(nchars)
            };
            let c1 = if row_x1 >= run_x1 {
                nchars
            } else {
                (((row_x1 - run_x0) / m.cw).ceil().max(0.0) as usize).min(nchars)
            };
            if c0 >= c1 {
                continue;
            }
            let sx0 = run_x0 + c0 as f32 * m.cw;
            let sx1 = run_x0 + c1 as f32 * m.cw;
            if let Some(pe) = prev_end_x
                && sx0 - pe >= m.cw * 0.65
            {
                line_txt.push(' ');
            }
            for ch in run.text.chars().skip(c0).take(c1 - c0) {
                line_txt.push(ch);
            }
            prev_end_x = Some(sx1);
            rect_x0 = Some(rect_x0.map_or(sx0, |a| a.min(sx0)));
            rect_x1 = rect_x1.max(sx1);
        }

        if let Some(rx0) = rect_x0
            && rect_x1 > rx0
        {
            rects.push([rx0, *ly, rect_x1, *ly + m.ch]);
            out_lines.push(line_txt);
        }
    }

    (rects, out_lines.join("\n"))
}

/// Build a [`TextSelection`] covering the word or full visual line at `(x, y)` (for double/triple click).
pub fn select_at_point(
    runs: &[TextRun],
    m: CellMetrics,
    x: f32,
    y: f32,
    whole_line: bool,
    min_y: f32,
) -> Option<TextSelection> {
    if m.cw <= 0.0 || m.ch <= 0.0 || y < min_y {
        return None;
    }
    let mut row: Vec<&TextRun> = runs
        .iter()
        .filter(|r| !r.text.is_empty() && y >= r.y - 2.0 && y <= r.y + m.ch + 2.0)
        .collect();
    if row.is_empty() {
        return None;
    }
    row.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(core::cmp::Ordering::Equal));
    let ly = row[0].y + m.ch * 0.5;
    if whole_line {
        let min_x = row.first()?.x;
        let last = row.last()?;
        let max_x = last.x + last.text.chars().count() as f32 * m.cw;
        return Some(TextSelection { anchor: [min_x, ly], head: [max_x, ly] });
    }
    for run in row {
        let chars: Vec<char> = run.text.chars().collect();
        let run_x1 = run.x + chars.len() as f32 * m.cw;
        if x >= run.x - m.cw * 0.25 && x <= run_x1 + m.cw * 0.25 && !chars.is_empty() {
            let idx = (((x - run.x) / m.cw).floor().max(0.0) as usize).min(chars.len() - 1);
            let is_word = |c: char| !c.is_whitespace() && !matches!(c, '(' | ')' | '[' | ']' | '{' | '}' | '"' | '\'' | ',' | ';');
            let want_word = is_word(chars[idx]);
            let mut s = idx;
            while s > 0 && is_word(chars[s - 1]) == want_word {
                s -= 1;
            }
            let mut e = idx + 1;
            while e < chars.len() && is_word(chars[e]) == want_word {
                e += 1;
            }
            return Some(TextSelection {
                anchor: [run.x + s as f32 * m.cw, ly],
                head: [run.x + e as f32 * m.cw, ly],
            });
        }
    }
    None
}

/// Complete layered frame output emitted by [`crate::layout::build_scene`].
#[derive(Clone, Debug)]
pub struct Scene {
    /// Background cards, headers, pills, alternating table rows, and borders.
    pub rects: Vec<RectInst>,
    /// Primary text runs rasterized over `rects`.
    pub text_runs: Vec<TextRun>,
    /// Retina RGBA SVG icons rasterized from `svg_icons::IconAtlas`.
    pub icon_quads: Vec<IconQuad>,
    /// Full-color Retina Apple Color Emoji / custom symbol quads rasterized via `IconAtlas`.
    pub emoji_quads: Vec<EmojiQuad>,
    /// Retina custom symbols registered via the Blaze Terminal API, rasterized via `IconAtlas`.
    pub custom_symbols: Vec<CustomSymbolQuad>,
    pub image_quads: Vec<ImageQuad>,
    /// Foreground overlays (underlines, error carets, beam cursor, hairlines) drawn over text.
    pub over_rects: Vec<RectInst>,
    /// Floating popup backgrounds (completion dropdown, Ctrl+R bar, drag chip) drawn above all content.
    pub overlay_rects: Vec<RectInst>,
    /// Floating popup Retina SVG icons drawn above `overlay_rects`.
    pub overlay_icons: Vec<IconQuad>,
    /// Floating popup Retina images / circular avatars drawn above `overlay_rects`.
    pub overlay_images: Vec<ImageQuad>,
    /// Floating popup text runs drawn above `overlay_icons`.
    pub overlay_text: Vec<TextRun>,
    /// Fragment-shader rounded-rect clip table (slot 0 is always the unbounded full viewport).
    pub clips: Vec<ClipRectGpu>,
    /// Clickable regions in paint order (`hit_test` walks in reverse so top-most wins).
    pub hit_regions: Vec<HitRegion>,
    /// Hoverable regions for interactive floating popovers (branch picker, stashes, dirty files, tooltips).
    pub hover_regions: Vec<HoverRegion>,
    /// Heading anchor targets `(github_slug, screen_y)` recorded during Markdown/HTML layout.
    pub anchors: Vec<(String, f32)>,
    /// Content-space top Y offset `(block_id, content_y)` for each block in the active pane.
    pub block_tops: Vec<(BlockId, f32)>,
    /// Bounding rect of the currently open floating popover (keeps popover open while pointer moves into it).
    pub popover_rect: Option<HitRect>,
    /// Embedded scrollable table viewports for nested vertical/horizontal trackpad scrolling.
    pub scroll_regions: Vec<ScrollRegion>,
    /// Draggable item regions (files, folders, crates) in paint order.
    pub drag_sources: Vec<DragSource>,
    /// Logical rect of the recessed composer input hotbar (for click-to-cursor & drag drop).
    pub composer_rect: HitRect,
    /// Logical rect of the currently running inline PTY block (when not fullscreen).
    pub inline_pty_rect: Option<HitRect>,
    /// Logical origin `[x, y]` of character `(0, 0)` inside the composer input hotbar.
    pub composer_text_origin: [f32; 2],
    /// Logical rect `[x0, y0, x1, y1]` of the live cursor in this frame (feeds Ghostty sweep trail).
    pub cursor_rect: Option<[f32; 4]>,
    /// Total height of the scrollable block column in logical pixels.
    pub total_content_height: f32,
    /// Height of the visible scroll viewport between top bar and input bar.
    pub scroll_viewport_height: f32,
    /// Y coordinate where the docked bottom input bar starts.
    pub input_top_y: f32,
    /// Window clear color RGB.
    pub clear_color: [f32; 3],
}

impl Scene {
    pub fn new() -> Self {
        let infinite_clip = ClipRectGpu {
            rect: [-1.0e6, -1.0e6, 1.0e6, 1.0e6],
            params: [0.0; 4],
        };
        Self {
            rects: Vec::new(),
            text_runs: Vec::new(),
            icon_quads: Vec::new(),
            emoji_quads: Vec::new(),
            custom_symbols: Vec::new(),
            image_quads: Vec::new(),
            over_rects: Vec::new(),
            overlay_rects: Vec::new(),
            overlay_icons: Vec::new(),
            overlay_images: Vec::new(),
            overlay_text: Vec::new(),
            clips: vec![infinite_clip],
            hit_regions: Vec::new(),
            hover_regions: Vec::new(),
            popover_rect: None,
            scroll_regions: Vec::new(),
            anchors: Vec::new(),
            block_tops: Vec::new(),
            drag_sources: Vec::new(),
            composer_rect: HitRect::new(0.0, 0.0, 0.0, 0.0),
            inline_pty_rect: None,
            composer_text_origin: [0.0, 0.0],
            cursor_rect: None,
            total_content_height: 0.0,
            scroll_viewport_height: 0.0,
            input_top_y: 0.0,
            clear_color: theme::BG_CANVAS_RGB,
        }
    }

    /// Push a rounded-rect clip region and return its GPU table index (`clip_id`).
    pub fn push_clip(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, radius: f32) -> u32 {
        let id = self.clips.len() as u32;
        self.clips.push(ClipRectGpu {
            rect: [x0, y0, x1, y1],
            params: [radius, 0.0, 0.0, 0.0],
        });
        id
    }

    #[allow(clippy::too_many_arguments)]
    pub fn round_rect(
        &mut self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        radius: f32,
        fill: [f32; 4],
        border_w: f32,
        border_color: [f32; 4],
        clip_id: u32,
    ) {
        self.rects.push(RectInst {
            rect: [x0, y0, x1, y1],
            color: fill,
            border_color,
            params: [radius, border_w, 0.0, prim_kind::ROUND_RECT],
            clip_info: [clip_id, 0, 0, 0],
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub fn overlay_round_rect(
        &mut self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        radius: f32,
        fill: [f32; 4],
        border_w: f32,
        border_color: [f32; 4],
        clip_id: u32,
    ) {
        self.overlay_rects.push(RectInst {
            rect: [x0, y0, x1, y1],
            color: fill,
            border_color,
            params: [radius, border_w, 0.0, prim_kind::ROUND_RECT],
            clip_info: [clip_id, 0, 0, 0],
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub fn shadow(
        &mut self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        radius: f32,
        blur: f32,
        color: [f32; 4],
        clip_id: u32,
    ) {
        self.rects.push(RectInst {
            rect: [x0, y0, x1, y1],
            color,
            border_color: [0.0; 4],
            params: [radius, 0.0, blur, prim_kind::SHADOW],
            clip_info: [clip_id, 0, 0, 0],
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub fn overlay_shadow(
        &mut self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        radius: f32,
        blur: f32,
        color: [f32; 4],
        clip_id: u32,
    ) {
        self.overlay_rects.push(RectInst {
            rect: [x0, y0, x1, y1],
            color,
            border_color: [0.0; 4],
            params: [radius, 0.0, blur, prim_kind::SHADOW],
            clip_info: [clip_id, 0, 0, 0],
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub fn over_round_rect(
        &mut self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        radius: f32,
        fill: [f32; 4],
        clip_id: u32,
    ) {
        self.over_rects.push(RectInst {
            rect: [x0, y0, x1, y1],
            color: fill,
            border_color: [0.0; 4],
            params: [radius, 0.0, 0.0, prim_kind::ROUND_RECT],
            clip_info: [clip_id, 0, 0, 0],
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub fn line(
        &mut self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        thickness: f32,
        color: [f32; 4],
        clip_id: u32,
    ) {
        self.over_rects.push(RectInst {
            rect: [x0, y0, x1, y1],
            color,
            border_color: [0.0; 4],
            params: [thickness, 0.0, 0.0, prim_kind::LINE],
            clip_info: [clip_id, 0, 0, 0],
        });
    }

    pub fn circle(
        &mut self,
        cx: f32,
        cy: f32,
        radius: f32,
        fill: [f32; 4],
        clip_id: u32,
    ) {
        self.rects.push(RectInst {
            rect: [cx - radius, cy - radius, cx + radius, cy + radius],
            color: fill,
            border_color: [0.0; 4],
            params: [radius, 0.0, 0.0, prim_kind::CIRCLE],
            clip_info: [clip_id, 0, 0, 0],
        });
    }

    pub fn over_circle(
        &mut self,
        cx: f32,
        cy: f32,
        radius: f32,
        fill: [f32; 4],
        clip_id: u32,
    ) {
        self.over_rects.push(RectInst {
            rect: [cx - radius, cy - radius, cx + radius, cy + radius],
            color: fill,
            border_color: [0.0; 4],
            params: [radius, 0.0, 0.0, prim_kind::CIRCLE],
            clip_info: [clip_id, 0, 0, 0],
        });
    }

    pub fn over_ring(
        &mut self,
        cx: f32,
        cy: f32,
        radius: f32,
        ring_width: f32,
        color: [f32; 4],
        clip_id: u32,
    ) {
        self.over_rects.push(RectInst {
            rect: [cx - radius, cy - radius, cx + radius, cy + radius],
            color,
            border_color: [0.0; 4],
            params: [radius, ring_width, 0.0, prim_kind::CIRCLE],
            clip_info: [clip_id, 0, 0, 0],
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub fn svg_icon(
        &mut self,
        x: f32,
        y: f32,
        size: f32,
        name: &'static str,
        clip_id: u32,
    ) {
        self.icon_quads.push(IconQuad { x, y, w: size, h: size, name, alpha: 1.0, clip_id });
    }

    #[allow(clippy::too_many_arguments)]
    pub fn svg_icon_alpha(
        &mut self,
        x: f32,
        y: f32,
        size: f32,
        name: &'static str,
        alpha: f32,
        clip_id: u32,
    ) {
        self.icon_quads.push(IconQuad { x, y, w: size, h: size, name, alpha, clip_id });
    }

    /// Draw a full-color Retina emoji or custom Unicode symbol (e.g. `"🦑"`, `"👋🌎"`), returning
    /// its logical horizontal width in pixels.
    #[allow(clippy::too_many_arguments)]
    pub fn emoji_symbol(
        &mut self,
        x: f32,
        y: f32,
        height: f32,
        text: &str,
        alpha: f32,
        clip_id: u32,
    ) -> f32 {
        let w = symbol_badge_width(text, height);
        self.emoji_quads.push(EmojiQuad {
            x,
            y,
            w,
            h: height,
            text: text.to_owned(),
            alpha,
            clip_id,
        });
        w
    }

    #[allow(clippy::too_many_arguments)]
    pub fn custom_symbol(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        name: String,
        tint: Option<[f32; 4]>,
        alpha: f32,
        clip_id: u32,
    ) {
        self.custom_symbols.push(CustomSymbolQuad { x, y, w, h, name, tint, alpha, clip_id });
    }

    #[allow(clippy::too_many_arguments)]
    pub fn image(&mut self, x: f32, y: f32, w: f32, h: f32, path: String, clip_id: u32) {
        self.image_quads.push(ImageQuad { x, y, w, h, path, alpha: 1.0, circle: false, clip_id });
    }

    #[allow(clippy::too_many_arguments)]
    pub fn avatar_image(&mut self, x: f32, y: f32, size: f32, path: String, clip_id: u32) {
        self.image_quads.push(ImageQuad {
            x,
            y,
            w: size,
            h: size,
            path,
            alpha: 1.0,
            circle: true,
            clip_id,
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub fn overlay_svg_icon(
        &mut self,
        x: f32,
        y: f32,
        size: f32,
        name: &'static str,
        clip_id: u32,
    ) {
        self.overlay_icons.push(IconQuad { x, y, w: size, h: size, name, alpha: 1.0, clip_id });
    }

    #[allow(clippy::too_many_arguments)]
    pub fn overlay_image(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        path: String,
        clip_id: u32,
    ) {
        self.overlay_images.push(ImageQuad {
            x,
            y,
            w,
            h,
            path,
            alpha: 1.0,
            circle: false,
            clip_id,
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub fn overlay_avatar_image(
        &mut self,
        x: f32,
        y: f32,
        size: f32,
        path: String,
        clip_id: u32,
    ) {
        self.overlay_images.push(ImageQuad {
            x,
            y,
            w: size,
            h: size,
            path,
            alpha: 1.0,
            circle: true,
            clip_id,
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub fn overlay_line(
        &mut self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        thickness: f32,
        color: [f32; 4],
        clip_id: u32,
    ) {
        self.overlay_rects.push(RectInst {
            rect: [x0, y0, x1, y1],
            color,
            border_color: [0.0; 4],
            params: [thickness, 0.0, 0.0, prim_kind::LINE],
            clip_info: [clip_id, 0, 0, 0],
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub fn text(
        &mut self,
        x: f32,
        y: f32,
        text: impl Into<String>,
        color: [f32; 4],
        bold: bool,
        italic: bool,
        clip_id: u32,
    ) {
        self.text_with_face(x, y, text, color, bold, italic, TextFace::Mono, clip_id);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn mono_small(
        &mut self,
        x: f32,
        y: f32,
        text: impl Into<String>,
        color: [f32; 4],
        bold: bool,
        italic: bool,
        clip_id: u32,
    ) {
        self.text_with_face(x, y, text, color, bold, italic, TextFace::MonoSmall, clip_id);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn ui_text(
        &mut self,
        x: f32,
        y: f32,
        text: impl Into<String>,
        color: [f32; 4],
        bold: bool,
        italic: bool,
        clip_id: u32,
    ) {
        self.text_with_face(x, y, text, color, bold, italic, TextFace::Ui, clip_id);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn ui_small(
        &mut self,
        x: f32,
        y: f32,
        text: impl Into<String>,
        color: [f32; 4],
        bold: bool,
        italic: bool,
        clip_id: u32,
    ) {
        self.text_with_face(x, y, text, color, bold, italic, TextFace::UiSmall, clip_id);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn heading_text(
        &mut self,
        x: f32,
        y: f32,
        text: impl Into<String>,
        color: [f32; 4],
        clip_id: u32,
    ) {
        self.text_with_face(x, y, text, color, true, false, TextFace::Heading, clip_id);
    }

    /// Fixed-size native macOS titlebar & pane-header SF Pro text (immune to `Cmd+`/`Cmd-` zoom).
    #[allow(clippy::too_many_arguments)]
    pub fn chrome_text(
        &mut self,
        x: f32,
        y: f32,
        text: impl Into<String>,
        color: [f32; 4],
        bold: bool,
        clip_id: u32,
    ) {
        self.text_with_face(x, y, text, color, bold, false, TextFace::Chrome, clip_id);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn text_with_face(
        &mut self,
        x: f32,
        y: f32,
        text: impl Into<String>,
        color: [f32; 4],
        bold: bool,
        italic: bool,
        face: TextFace,
        clip_id: u32,
    ) {
        let s = text.into();
        if !s.is_empty() {
            self.text_runs.push(TextRun { x, y, text: s, color, bold, italic, face, clip_id });
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn overlay_text_run(
        &mut self,
        x: f32,
        y: f32,
        text: impl Into<String>,
        color: [f32; 4],
        bold: bool,
        italic: bool,
        clip_id: u32,
    ) {
        self.overlay_text_with_face(x, y, text, color, bold, italic, TextFace::Mono, clip_id);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn overlay_ui_text(
        &mut self,
        x: f32,
        y: f32,
        text: impl Into<String>,
        color: [f32; 4],
        bold: bool,
        italic: bool,
        clip_id: u32,
    ) {
        self.overlay_text_with_face(x, y, text, color, bold, italic, TextFace::Ui, clip_id);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn overlay_mono_small(
        &mut self,
        x: f32,
        y: f32,
        text: impl Into<String>,
        color: [f32; 4],
        bold: bool,
        italic: bool,
        clip_id: u32,
    ) {
        self.overlay_text_with_face(x, y, text, color, bold, italic, TextFace::MonoSmall, clip_id);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn overlay_ui_small(
        &mut self,
        x: f32,
        y: f32,
        text: impl Into<String>,
        color: [f32; 4],
        bold: bool,
        italic: bool,
        clip_id: u32,
    ) {
        self.overlay_text_with_face(x, y, text, color, bold, italic, TextFace::UiSmall, clip_id);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn overlay_text_with_face(
        &mut self,
        x: f32,
        y: f32,
        text: impl Into<String>,
        color: [f32; 4],
        bold: bool,
        italic: bool,
        face: TextFace,
        clip_id: u32,
    ) {
        let s = text.into();
        if !s.is_empty() {
            self.overlay_text.push(TextRun { x, y, text: s, color, bold, italic, face, clip_id });
        }
    }

    pub fn push_hit(&mut self, rect: HitRect, action: HitAction) {
        self.hit_regions.push(HitRegion { rect, action });
    }

    /// Record a hit target clipped against `self.clips[clip_id]` so scrolled-off buttons
    /// behind the top bar or docked input card can never intercept mouse clicks.
    pub fn push_hit_clipped(&mut self, rect: HitRect, clip_id: u32, action: HitAction) {
        let Some(clip) = self.clips.get(clip_id as usize) else {
            self.hit_regions.push(HitRegion { rect, action });
            return;
        };
        let x0 = rect.x0.max(clip.rect[0]);
        let y0 = rect.y0.max(clip.rect[1]);
        let x1 = rect.x1.min(clip.rect[2]);
        let y1 = rect.y1.min(clip.rect[3]);
        if x1 > x0 && y1 > y0 {
            self.hit_regions.push(HitRegion {
                rect: HitRect::new(x0, y0, x1, y1),
                action,
            });
        }
    }

    /// Hit-test a logical point `(x, y)` against recorded regions (top-most wins).
    pub fn hit_test(&self, x: f32, y: f32) -> Option<HitAction> {
        for region in self.hit_regions.iter().rev() {
            if region.rect.contains(x, y) {
                return Some(region.action.clone());
            }
        }
        None
    }

    /// Record a hoverable region clipped against `self.clips[clip_id]`.
    pub fn push_hover_clipped(&mut self, rect: HitRect, clip_id: u32, kind: HoverKind) {
        let Some(clip) = self.clips.get(clip_id as usize) else {
            self.hover_regions.push(HoverRegion { rect, kind });
            return;
        };
        let x0 = rect.x0.max(clip.rect[0]);
        let y0 = rect.y0.max(clip.rect[1]);
        let x1 = rect.x1.min(clip.rect[2]);
        let y1 = rect.y1.min(clip.rect[3]);
        if x1 > x0 && y1 > y0 {
            self.hover_regions.push(HoverRegion {
                rect: HitRect::new(x0, y0, x1, y1),
                kind,
            });
        }
    }

    /// Find the top-most hoverable region under `(x, y)`.
    pub fn hover_at(&self, x: f32, y: f32) -> Option<&HoverRegion> {
        for reg in self.hover_regions.iter().rev() {
            if reg.rect.contains(x, y) {
                return Some(reg);
            }
        }
        None
    }

    /// Record a scrollable table viewport clipped against `self.clips[clip_id]`.
    pub fn push_scroll_region(
        &mut self,
        rect: HitRect,
        clip_id: u32,
        block: BlockId,
        max_scroll_y: f32,
        max_scroll_x: f32,
    ) {
        let Some(clip) = self.clips.get(clip_id as usize) else {
            self.scroll_regions.push(ScrollRegion { rect, block, max_scroll_y, max_scroll_x });
            return;
        };
        let x0 = rect.x0.max(clip.rect[0]);
        let y0 = rect.y0.max(clip.rect[1]);
        let x1 = rect.x1.min(clip.rect[2]);
        let y1 = rect.y1.min(clip.rect[3]);
        if x1 > x0 && y1 > y0 {
            self.scroll_regions.push(ScrollRegion {
                rect: HitRect::new(x0, y0, x1, y1),
                block,
                max_scroll_y,
                max_scroll_x,
            });
        }
    }

    /// Find the top-most scrollable table viewport under `(x, y)`.
    pub fn scroll_region_at(&self, x: f32, y: f32) -> Option<ScrollRegion> {
        for reg in self.scroll_regions.iter().rev() {
            if reg.rect.contains(x, y) {
                return Some(*reg);
            }
        }
        None
    }

    /// Record a draggable item clipped against `self.clips[clip_id]`.
    pub fn push_drag_source_clipped(&mut self, rect: HitRect, clip_id: u32, payload: DragPayload) {
        let Some(clip) = self.clips.get(clip_id as usize) else {
            self.drag_sources.push(DragSource { rect, payload });
            return;
        };
        let x0 = rect.x0.max(clip.rect[0]);
        let y0 = rect.y0.max(clip.rect[1]);
        let x1 = rect.x1.min(clip.rect[2]);
        let y1 = rect.y1.min(clip.rect[3]);
        if x1 > x0 && y1 > y0 {
            self.drag_sources.push(DragSource {
                rect: HitRect::new(x0, y0, x1, y1),
                payload,
            });
        }
    }

    /// Find the top-most draggable item under `(x, y)`.
    pub fn drag_source_at(&self, x: f32, y: f32) -> Option<&DragPayload> {
        for src in self.drag_sources.iter().rev() {
            if src.rect.contains(x, y) {
                return Some(&src.payload);
            }
        }
        None
    }

    /// Composite a full pane sub-scene (rendered for `[0..pane_w, top_offset..top_offset+pane_h]`)
    /// cleanly into `pane_rect` inside the window's master [`Scene`].
    ///
    /// Base layers are tightly clipped to `pane_rect` (zero bleed between tiled panes); floating
    /// overlays from the focused pane (`Ctrl+R`, hover previews, completions) float freely above
    /// pane borders inside the full window viewport.
    pub fn composite_pane(
        &mut self,
        sub: Scene,
        pane_rect: HitRect,
        top_offset: f32,
        is_focused: bool,
        win_w: f32,
        win_h: f32,
    ) {
        let dx = pane_rect.x0;
        let dy = pane_rect.y0 - top_offset;
        let pane_clip = self.push_clip(pane_rect.x0, pane_rect.y0, pane_rect.x1, pane_rect.y1, 0.0);

        let mut clip_map: Vec<u32> = Vec::with_capacity(sub.clips.len());
        let mut overlay_clip_map: Vec<u32> = Vec::with_capacity(sub.clips.len());
        for (i, c) in sub.clips.iter().enumerate() {
            if i == 0 {
                clip_map.push(pane_clip);
                overlay_clip_map.push(0);
            } else {
                let cx0 = (c.rect[0] + dx).max(pane_rect.x0);
                let cy0 = (c.rect[1] + dy).max(pane_rect.y0);
                let cx1 = (c.rect[2] + dx).min(pane_rect.x1).max(cx0);
                let cy1 = (c.rect[3] + dy).min(pane_rect.y1).max(cy0);
                let id = self.push_clip(cx0, cy0, cx1, cy1, c.params[0]);
                clip_map.push(id);

                if is_focused {
                    let ox0 = (c.rect[0] + dx).max(0.0);
                    let oy0 = (c.rect[1] + dy).max(0.0);
                    let ox1 = (c.rect[2] + dx).min(win_w).max(ox0);
                    let oy1 = (c.rect[3] + dy).min(win_h).max(oy0);
                    overlay_clip_map.push(self.push_clip(ox0, oy0, ox1, oy1, c.params[0]));
                } else {
                    overlay_clip_map.push(0);
                }
            }
        }
        let map_clip = |cid: u32| -> u32 {
            clip_map.get(cid as usize).copied().unwrap_or(pane_clip)
        };
        let map_ov_clip = |cid: u32| -> u32 {
            overlay_clip_map.get(cid as usize).copied().unwrap_or(0)
        };
        let xlat_rect = |mut r: RectInst, cid: u32| -> RectInst {
            r.rect[0] += dx;
            r.rect[1] += dy;
            r.rect[2] += dx;
            r.rect[3] += dy;
            r.clip_info[0] = cid;
            r
        };
        let xlat_hit = |r: HitRect| -> Option<HitRect> {
            let x0 = (r.x0 + dx).max(pane_rect.x0);
            let y0 = (r.y0 + dy).max(pane_rect.y0);
            let x1 = (r.x1 + dx).min(pane_rect.x1);
            let y1 = (r.y1 + dy).min(pane_rect.y1);
            (x1 > x0 && y1 > y0).then_some(HitRect::new(x0, y0, x1, y1))
        };

        for r in sub.rects {
            let cid = map_clip(r.clip_info[0]);
            self.rects.push(xlat_rect(r, cid));
        }
        for mut t in sub.text_runs {
            t.x += dx;
            t.y += dy;
            t.clip_id = map_clip(t.clip_id);
            self.text_runs.push(t);
        }
        for mut ic in sub.icon_quads {
            ic.x += dx;
            ic.y += dy;
            ic.clip_id = map_clip(ic.clip_id);
            self.icon_quads.push(ic);
        }
        for mut em in sub.emoji_quads {
            em.x += dx;
            em.y += dy;
            em.clip_id = map_clip(em.clip_id);
            self.emoji_quads.push(em);
        }
        for mut cs in sub.custom_symbols {
            cs.x += dx;
            cs.y += dy;
            cs.clip_id = map_clip(cs.clip_id);
            self.custom_symbols.push(cs);
        }
        for mut img in sub.image_quads {
            img.x += dx;
            img.y += dy;
            img.clip_id = map_clip(img.clip_id);
            self.image_quads.push(img);
        }
        for r in sub.over_rects {
            let cid = map_clip(r.clip_info[0]);
            self.over_rects.push(xlat_rect(r, cid));
        }

        for h in sub.hit_regions {
            if let Some(rect) = xlat_hit(h.rect) {
                self.hit_regions.push(HitRegion { rect, action: h.action });
            }
        }
        for hv in sub.hover_regions {
            if let Some(rect) = xlat_hit(hv.rect) {
                self.hover_regions.push(HoverRegion { rect, kind: hv.kind });
            }
        }
        for sc in sub.scroll_regions {
            if let Some(rect) = xlat_hit(sc.rect) {
                self.scroll_regions.push(ScrollRegion {
                    rect,
                    block: sc.block,
                    max_scroll_y: sc.max_scroll_y,
                    max_scroll_x: sc.max_scroll_x,
                });
            }
        }
        for ds in sub.drag_sources {
            if let Some(rect) = xlat_hit(ds.rect) {
                self.drag_sources.push(DragSource { rect, payload: ds.payload });
            }
        }

        if is_focused {
            for (slug, ay) in sub.anchors {
                self.anchors.push((slug, ay + dy));
            }
            for r in sub.overlay_rects {
                let cid = map_ov_clip(r.clip_info[0]);
                self.overlay_rects.push(xlat_rect(r, cid));
            }
            for mut ic in sub.overlay_icons {
                ic.x += dx;
                ic.y += dy;
                ic.clip_id = map_ov_clip(ic.clip_id);
                self.overlay_icons.push(ic);
            }
            for mut img in sub.overlay_images {
                img.x += dx;
                img.y += dy;
                img.clip_id = map_ov_clip(img.clip_id);
                self.overlay_images.push(img);
            }
            for mut t in sub.overlay_text {
                t.x += dx;
                t.y += dy;
                t.clip_id = map_ov_clip(t.clip_id);
                self.overlay_text.push(t);
            }
            self.composer_rect = HitRect::new(
                sub.composer_rect.x0 + dx,
                sub.composer_rect.y0 + dy,
                sub.composer_rect.x1 + dx,
                sub.composer_rect.y1 + dy,
            );
            self.inline_pty_rect = sub
                .inline_pty_rect
                .map(|r| HitRect::new(r.x0 + dx, r.y0 + dy, r.x1 + dx, r.y1 + dy));
            self.composer_text_origin = [
                sub.composer_text_origin[0] + dx,
                sub.composer_text_origin[1] + dy,
            ];
            self.cursor_rect = sub.cursor_rect.map(|c| [c[0] + dx, c[1] + dy, c[2] + dx, c[3] + dy]);
            self.popover_rect = sub
                .popover_rect
                .map(|r| HitRect::new(r.x0 + dx, r.y0 + dy, r.x1 + dx, r.y1 + dy));
            self.total_content_height = sub.total_content_height;
            self.scroll_viewport_height = sub.scroll_viewport_height;
            self.input_top_y = sub.input_top_y + dy;
        }
    }
}

impl Default for Scene {
    fn default() -> Self {
        Self::new()
    }
}

/// Compact cell representation of a `Value` for table cells and badges.
pub fn format_value_cell(val: &Value<'_>) -> String {
    format_value_cell_at(val, None)
}

/// Compact cell representation, formatting [`Value::Date`] as human relative time
/// (`3m ago`, `2h ago`, `Nov 24`) when `now_ns` is supplied.
pub fn format_value_cell_at(val: &Value<'_>, now_ns: Option<i64>) -> String {
    match val {
        Value::List(l) => format!("[list {}]", l.len()),
        Value::Record(r) => format!("{{record {}}}", r.len()),
        Value::Table(t) => format!("[table {}x{}]", t.nrows(), t.ncols()),
        Value::Nothing => "—".to_owned(),
        Value::Date(dt) => {
            let mut out = String::new();
            if let Some(now) = now_ns {
                let _ = blaze_sh::format_relative_time(*dt, now, &mut out);
            } else {
                let _ = val.format_cell(&mut out);
            }
            out
        }
        _ => {
            let mut out = String::new();
            let _ = val.format_cell(&mut out);
            out
        }
    }
}

/// True when clicking a table cell containing `val` should expand/collapse an inline drawer.
pub fn is_expandable_cell(val: &Value<'_>) -> bool {
    matches!(val, Value::List(_) | Value::Record(_) | Value::Table(_))
}

/// Format elapsed milliseconds concisely (`<1ms`, `4ms`, `1.2s`).
pub fn format_duration_ms(ms: u64) -> String {
    if ms == 0 {
        "<1ms".to_owned()
    } else if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        let secs = ms / 1_000;
        let tenths = (ms % 1_000) / 100;
        format!("{secs}.{tenths}s")
    } else {
        let mins = ms / 60_000;
        let secs = (ms % 60_000) / 1_000;
        format!("{mins}m {secs}s")
    }
}

/// Format exact elapsed nanoseconds concisely (`420ns`, `142µs`, `2.4ms`, `37ms`, `1.4s`, `2m 14s`).
pub fn format_duration_ns(ns: u64) -> String {
    if ns < 1_000 {
        format!("{ns}ns")
    } else if ns < 1_000_000 {
        let us = ns / 1_000;
        format!("{us}µs")
    } else if ns < 10_000_000 {
        let ms = ns / 1_000_000;
        let tenths = (ns % 1_000_000) / 100_000;
        if tenths == 0 {
            format!("{ms}ms")
        } else {
            format!("{ms}.{tenths}ms")
        }
    } else {
        format_duration_ms(ns / 1_000_000)
    }
}

/// Middle-truncate `s` (e.g. long paths `crates/blaze-shell/src/layout.rs` -> `crates/…/layout.rs`)
/// so the filename at the end stays readable inside narrow tiled panes!
pub fn truncate_middle(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        return s.to_owned();
    }
    if max_chars <= 3 {
        return truncate_chars(s, max_chars);
    }
    let keep_tail = ((max_chars - 1) * 2 / 3).max(4).min(max_chars - 2);
    let keep_head = max_chars - 1 - keep_tail;
    let mut out = String::with_capacity(s.len());
    for &c in &chars[..keep_head] {
        out.push(c);
    }
    out.push('…');
    for &c in &chars[chars.len() - keep_tail..] {
        out.push(c);
    }
    out
}

/// Truncate `s` to at most `max_chars` Unicode scalar values, appending `…` if truncated.
pub fn truncate_chars(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_owned();
    }
    if max_chars == 1 {
        return "…".to_owned();
    }
    let mut out: String = s.chars().take(max_chars - 1).collect();
    out.push('…');
    out
}

/// Wrap `s` into visual lines of at most `max_cols` characters each (preserving explicit `\n`).
pub fn wrap_lines(s: &str, max_cols: usize) -> Vec<String> {
    let cap = max_cols.max(8);
    let mut out = Vec::new();
    for raw_line in s.split('\n') {
        let cleaned = raw_line.trim_end_matches('\r');
        if cleaned.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut cur = String::new();
        let mut cur_len = 0usize;
        for ch in cleaned.chars() {
            if cur_len >= cap {
                out.push(core::mem::take(&mut cur));
                cur_len = 0;
            }
            cur.push(ch);
            cur_len += 1;
        }
        if !cur.is_empty() {
            out.push(cur);
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}
