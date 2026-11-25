//! GPU-independent visual scene primitives, hit-test regions, and structured value
//! formatting helpers consumed by `layout.rs` and rasterized by Metal (`render.rs`).

use blaze_sh::Value;
use blaze_ui::shaders::{ClipRectGpu, RectInst, prim_kind};

use crate::blocks::{BlockId, BranchId};
use crate::theme;

/// Logical CSS-pixel dimensions of one monospace cell.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellMetrics {
    pub cw: f32,
    pub ch: f32,
}

impl Default for CellMetrics {
    fn default() -> Self {
        Self { cw: 8.0, ch: 18.0 }
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

/// Semantic UI action triggered by clicking a [`HitRegion`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HitAction {
    SwitchBranch(BranchId),
    ForkActiveBranch,
    ForkBlock(BlockId),
    ToggleFold(BlockId),
    ToggleTableView(BlockId),
    ToggleExpandCell { block: BlockId, row: usize, col: usize },
    SelectCompletion(usize),
    SelectHistory(usize),
    CycleHistoryScope,
    ResetZoom,
}

/// Clickable UI target recorded during layout.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HitRegion {
    pub rect: HitRect,
    pub action: HitAction,
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
    /// Foreground overlays (underlines, error carets, beam cursor, hairlines) drawn over text.
    pub over_rects: Vec<RectInst>,
    /// Floating popup backgrounds (completion dropdown, Ctrl+R bar, drag chip) drawn above all content.
    pub overlay_rects: Vec<RectInst>,
    /// Floating popup Retina SVG icons drawn above `overlay_rects`.
    pub overlay_icons: Vec<IconQuad>,
    /// Floating popup text runs drawn above `overlay_icons`.
    pub overlay_text: Vec<TextRun>,
    /// Fragment-shader rounded-rect clip table (slot 0 is always the unbounded full viewport).
    pub clips: Vec<ClipRectGpu>,
    /// Clickable regions in paint order (`hit_test` walks in reverse so top-most wins).
    pub hit_regions: Vec<HitRegion>,
    /// Draggable item regions (files, folders, crates) in paint order.
    pub drag_sources: Vec<DragSource>,
    /// Logical rect of the recessed composer input hotbar (for click-to-cursor & drag drop).
    pub composer_rect: HitRect,
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
            over_rects: Vec::new(),
            overlay_rects: Vec::new(),
            overlay_icons: Vec::new(),
            overlay_text: Vec::new(),
            clips: vec![infinite_clip],
            hit_regions: Vec::new(),
            drag_sources: Vec::new(),
            composer_rect: HitRect::new(0.0, 0.0, 0.0, 0.0),
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
        let s = text.into();
        if !s.is_empty() {
            self.text_runs.push(TextRun { x, y, text: s, color, bold, italic, clip_id });
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
        let s = text.into();
        if !s.is_empty() {
            self.overlay_text.push(TextRun { x, y, text: s, color, bold, italic, clip_id });
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
                return Some(region.action);
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
