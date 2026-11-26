//! Reusable Metal UI primitives: scrollable viewports, scrollbars, section headers,
//! pill buttons, and status badges.

use crate::theme;
use crate::view::{CellMetrics, HitAction, HitRect, Scene, TextFace};

/// Reusable vertical scroll state for any clipped list or preview viewport.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScrollState {
    pub offset_y: f32,
    pub content_h: f32,
    pub viewport_h: f32,
}

impl ScrollState {
    pub const fn new() -> Self {
        Self { offset_y: 0.0, content_h: 0.0, viewport_h: 0.0 }
    }

    #[inline]
    pub fn max_scroll(&self) -> f32 {
        (self.content_h - self.viewport_h).max(0.0)
    }

    #[inline]
    pub fn is_scrollable(&self) -> bool {
        self.max_scroll() > 1.0
    }

    pub fn clamp(&mut self) {
        let max = self.max_scroll();
        if !self.offset_y.is_finite() || self.offset_y < 0.0 {
            self.offset_y = 0.0;
        } else if self.offset_y > max {
            self.offset_y = max;
        }
    }

    pub fn set_bounds(&mut self, content_h: f32, viewport_h: f32) {
        self.content_h = content_h.max(0.0);
        self.viewport_h = viewport_h.max(0.0);
        self.clamp();
    }

    /// Scroll vertically by `delta_px` (positive moves content up / reveals lower items).
    /// Returns `true` if `offset_y` changed.
    pub fn scroll_by(&mut self, delta_px: f32) -> bool {
        let prev = self.offset_y;
        self.offset_y += delta_px;
        self.clamp();
        (self.offset_y - prev).abs() > 0.01
    }

    /// Ensure an item spanning `[item_y0, item_y1]` (in content coordinates relative to top of
    /// content = 0) is visible inside `[offset_y, offset_y + viewport_h]`, with `padding` px margin.
    pub fn ensure_visible(&mut self, item_y0: f32, item_y1: f32, padding: f32) -> bool {
        if self.viewport_h <= 1.0 {
            return false;
        }
        let prev = self.offset_y;
        let top = item_y0 - padding;
        let bot = item_y1 + padding;
        if top < self.offset_y {
            self.offset_y = top;
        } else if bot > self.offset_y + self.viewport_h {
            self.offset_y = bot - self.viewport_h;
        }
        self.clamp();
        (self.offset_y - prev).abs() > 0.01
    }
}

/// Draw a subtle vertical scrollbar track + proportional thumb when `state.is_scrollable()`.
#[allow(clippy::too_many_arguments)]
pub fn draw_scrollbar(
    scene: &mut Scene,
    state: ScrollState,
    track_x0: f32,
    track_y0: f32,
    track_x1: f32,
    track_y1: f32,
    clip_id: u32,
) {
    if !state.is_scrollable() || track_y1 <= track_y0 + 8.0 || track_x1 <= track_x0 {
        return;
    }
    let track_h = track_y1 - track_y0;
    let radius = (track_x1 - track_x0) * 0.5;

    scene.over_round_rect(
        track_x0,
        track_y0,
        track_x1,
        track_y1,
        radius,
        theme::rgba(0xff, 0xff, 0xff, 0.05),
        clip_id,
    );

    let visible_ratio = (state.viewport_h / state.content_h.max(1.0)).clamp(0.0, 1.0);
    let thumb_h = (track_h * visible_ratio).max(24.0).min(track_h);
    let max_scroll = state.max_scroll().max(1.0);
    let scroll_frac = (state.offset_y / max_scroll).clamp(0.0, 1.0);
    let thumb_y0 = track_y0 + (track_h - thumb_h) * scroll_frac;
    let thumb_y1 = thumb_y0 + thumb_h;

    scene.over_round_rect(
        track_x0,
        thumb_y0,
        track_x1,
        thumb_y1,
        radius,
        theme::rgba(0xeb, 0xea, 0xe6, 0.28),
        clip_id,
    );
}

/// Draw a compact section header with bold `label`, optional right-aligned `count_badge`,
/// and a subtle bottom hairline divider. Returns the consumed vertical height.
#[allow(clippy::too_many_arguments)]
pub fn draw_section_header(
    scene: &mut Scene,
    x0: f32,
    x1: f32,
    y: f32,
    label: &str,
    count_badge: Option<&str>,
    m: CellMetrics,
    clip_id: u32,
) -> f32 {
    let text_h = m.line_h(TextFace::UiSmall);
    let h = text_h + 10.0;
    let ty = y + (h - 2.0 - text_h) * 0.5;

    scene.ui_small(x0, ty, label, theme::INK_3, true, false, clip_id);

    if let Some(badge) = count_badge
        && !badge.is_empty()
    {
        let bw = m.measure(badge, TextFace::UiSmall);
        let bx = (x1 - bw).max(x0);
        scene.ui_small(bx, ty, badge, theme::INK_4, false, false, clip_id);
    }

    scene.line(x0, y + h - 2.0, x1, y + h - 2.0, 1.0, theme::LINE_FAINT, clip_id);
    h
}

/// Draw a clickable pill button and register its clipped hit target. Returns the button width.
#[allow(clippy::too_many_arguments)]
pub fn draw_pill_button(
    scene: &mut Scene,
    x0: f32,
    y0: f32,
    label: &str,
    fg: [f32; 4],
    bg: [f32; 4],
    border: [f32; 4],
    bold: bool,
    m: CellMetrics,
    clip_id: u32,
    action: HitAction,
) -> f32 {
    let text_h = m.line_h(TextFace::UiSmall);
    let h = text_h + 7.0;
    let w = m.measure_styled(label, TextFace::UiSmall, bold) + 16.0;
    let border_w = if border[3] > 0.0 { 1.0 } else { 0.0 };

    scene.round_rect(x0, y0, x0 + w, y0 + h, 5.0, bg, border_w, border, clip_id);

    let ty = y0 + (h - text_h) * 0.5;
    scene.ui_small(x0 + 8.0, ty, label, fg, bold, false, clip_id);

    scene.push_hit_clipped(HitRect::new(x0, y0, x0 + w, y0 + h), clip_id, action);
    w
}

/// Draw a rounded status badge pill with a left status dot and `UiSmall` label.
/// Returns the pill width.
#[allow(clippy::too_many_arguments)]
pub fn draw_status_pill(
    scene: &mut Scene,
    x0: f32,
    y0: f32,
    label: &str,
    dot_col: [f32; 4],
    fg: [f32; 4],
    bg: [f32; 4],
    border: [f32; 4],
    m: CellMetrics,
    clip_id: u32,
) -> f32 {
    let text_h = m.line_h(TextFace::UiSmall);
    let h = text_h + 6.0;
    let radius = h * 0.5;
    let dot_r = 2.6;
    let pad_l = 8.0;
    let gap = 5.0;
    let pad_r = 9.0;

    let dot_cx = x0 + pad_l + dot_r;
    let text_x = dot_cx + dot_r + gap;
    let text_w = m.measure_styled(label, TextFace::UiSmall, true);
    let w = (text_x - x0) + text_w + pad_r;
    let border_w = if border[3] > 0.0 { 1.0 } else { 0.0 };

    scene.round_rect(x0, y0, x0 + w, y0 + h, radius, bg, border_w, border, clip_id);
    scene.over_circle(dot_cx, y0 + h * 0.5, dot_r, dot_col, clip_id);

    let ty = y0 + (h - text_h) * 0.5;
    scene.ui_small(text_x, ty, label, fg, true, false, clip_id);
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_state_bounds_and_clamp() {
        let mut s = ScrollState::new();
        assert_eq!(s.max_scroll(), 0.0);
        assert!(!s.is_scrollable());

        s.set_bounds(500.0, 200.0);
        assert_eq!(s.max_scroll(), 300.0);
        assert!(s.is_scrollable());

        s.offset_y = 450.0;
        s.clamp();
        assert_eq!(s.offset_y, 300.0);

        s.offset_y = -25.0;
        s.clamp();
        assert_eq!(s.offset_y, 0.0);

        // NaN and infinite offsets must reset cleanly to 0.
        s.offset_y = f32::NAN;
        s.clamp();
        assert_eq!(s.offset_y, 0.0);

        s.offset_y = f32::INFINITY;
        s.clamp();
        assert_eq!(s.offset_y, 0.0);

        s.offset_y = f32::NEG_INFINITY;
        s.clamp();
        assert_eq!(s.offset_y, 0.0);

        // Negative bounds clamp to zero.
        s.set_bounds(-50.0, -10.0);
        assert_eq!(s.content_h, 0.0);
        assert_eq!(s.viewport_h, 0.0);
        assert_eq!(s.max_scroll(), 0.0);
        assert!(!s.is_scrollable());
    }

    #[test]
    fn scroll_state_scroll_by() {
        let mut s = ScrollState::new();
        s.set_bounds(400.0, 100.0);

        assert!(s.scroll_by(60.0));
        assert_eq!(s.offset_y, 60.0);

        assert!(s.scroll_by(500.0));
        assert_eq!(s.offset_y, 300.0);

        // Further scrolling past bottom reports no change.
        assert!(!s.scroll_by(50.0));
        assert_eq!(s.offset_y, 300.0);

        assert!(s.scroll_by(-400.0));
        assert_eq!(s.offset_y, 0.0);

        // Further scrolling past top reports no change.
        assert!(!s.scroll_by(-10.0));
        assert_eq!(s.offset_y, 0.0);
    }

    #[test]
    fn scroll_state_ensure_visible() {
        let mut s = ScrollState::new();
        // Zero viewport returns false without moving.
        assert!(!s.ensure_visible(50.0, 90.0, 8.0));

        s.set_bounds(600.0, 200.0);
        s.offset_y = 100.0;

        // Already fully inside [100, 300] with padding 8 -> no scroll.
        assert!(!s.ensure_visible(120.0, 180.0, 8.0));
        assert_eq!(s.offset_y, 100.0);

        // Below viewport: item [310, 350] with padding 10 -> bot=360, offset_y=160.
        assert!(s.ensure_visible(310.0, 350.0, 10.0));
        assert_eq!(s.offset_y, 160.0);

        // Above viewport: item [40, 80] with padding 10 -> top=30, offset_y=30.
        assert!(s.ensure_visible(40.0, 80.0, 10.0));
        assert_eq!(s.offset_y, 30.0);

        // Near very top clamps to 0.
        assert!(s.ensure_visible(2.0, 20.0, 10.0));
        assert_eq!(s.offset_y, 0.0);

        // Near very bottom clamps to max_scroll (400).
        assert!(s.ensure_visible(580.0, 600.0, 20.0));
        assert_eq!(s.offset_y, 400.0);
    }

    #[test]
    fn widget_draw_primitives_emit_expected_geometry() {
        let m = CellMetrics::default();
        let mut scene = Scene::new();

        // Non-scrollable scrollbar draws nothing.
        let mut s = ScrollState::new();
        s.set_bounds(100.0, 200.0);
        draw_scrollbar(&mut scene, s, 96.0, 10.0, 100.0, 190.0, 0);
        assert!(scene.over_rects.is_empty());

        // Scrollable scrollbar draws track + thumb.
        s.set_bounds(800.0, 200.0);
        s.scroll_by(300.0);
        draw_scrollbar(&mut scene, s, 96.0, 10.0, 100.0, 190.0, 0);
        assert_eq!(scene.over_rects.len(), 2);

        // Section header returns line_h + 10 and emits label, badge, and divider.
        let hdr_h = draw_section_header(&mut scene, 12.0, 240.0, 20.0, "LOCAL", Some("3"), m, 0);
        assert!((hdr_h - (m.line_h(TextFace::UiSmall) + 10.0)).abs() < 0.01);
        assert!(scene.text_runs.iter().any(|t| t.text == "LOCAL" && t.bold));
        assert!(scene.text_runs.iter().any(|t| t.text == "3" && !t.bold));

        // Pill button registers clipped hit region.
        let bw = draw_pill_button(
            &mut scene,
            12.0,
            60.0,
            "New",
            theme::INK_1,
            theme::BG_CHIP,
            theme::LINE,
            true,
            m,
            0,
            HitAction::NewTab,
        );
        assert!(bw > 16.0);
        assert_eq!(scene.hit_test(16.0, 66.0), Some(HitAction::NewTab));

        // Status pill emits rounded pill + dot + label.
        let over_before = scene.over_rects.len();
        let sw = draw_status_pill(
            &mut scene,
            12.0,
            100.0,
            "working",
            theme::OK,
            theme::OK,
            theme::BG_CHIP,
            theme::LINE,
            m,
            0,
        );
        assert!(sw > 27.0);
        assert_eq!(scene.over_rects.len(), over_before + 1);
        assert!(scene.text_runs.iter().any(|t| t.text == "working"));
    }
}
