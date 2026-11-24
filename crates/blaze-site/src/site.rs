//! Pure-Blaze sans-IO runtime engine for the Blaze front page (`BlazeSite`).
//!
//! Combines:
//! - Relational `blaze_ecs::World` (`ChildOf` + Flecs 4.2 `DontFragment` hover/focus/press tags)
//! - Glitch-free `blaze_ecs::ReactiveGraph` (`Signal<bool>` theme, `Signal<f32>` zoom, hover/find)
//! - `den` Slug quadratic Bézier outline font rendering (`SlugFontTable` + `gpu::slug::SiteInst`)
//! - Smooth cursor-anchored trackpad pinch-to-zoom (`0.25x..=16.0x`) + 2-finger pan/scroll
//! - Liquid multi-line selection (`selshape`) + 300ms copy-pulse sweep + double/triple click
//! - Floating hover cards (`layout_tip`), footnote jump flash, 60ms scrambled `ucki` glyphs
//! - Native WebGPU `Cmd+F` Find Bar + Zoom HUD Pill rendered in the exact same single draw call!

use core::alloc::Allocator;
use core::f32::consts::PI;
use core::fmt::Write as _;

use blaze_ecs::{Entity, ReactiveGraph, Signal, World};
use blaze_own::{String as OwnString, Vec as OwnVec};
use blaze_ui::slug::SlugFontTable;
use blaze_ui::slug_shader::{SiteInst, SiteUniform, site_kind};
use blaze_ui::types::{Rect, Rgba};
use gpu::prelude::{UVec4, Vec4, uvec4, vec4};

const ZERO_V4: Vec4 = Vec4::splat(0.0);
const ZERO_U4: UVec4 = UVec4::splat(0);

use crate::error::SiteError;
use crate::layout::{
    CharRange, MAX_COL_WIDTH, PageLayout, Pic, PlacedGlyph, Shape, ShapeKind, TipLayout, Viewport,
    block_item_range, char_at_point, copy_text, find_matches, layout_page, layout_tip, word_range,
};
use crate::model::{
    Block, PageModel, SiteEcsHandles, SiteFocused, SiteHovered, SitePressed,
};
use crate::palette::{ImageName, SiteTheme, Token};
use crate::selshape::{
    PULSE_MS, ROW_IN_MS, Row, STAGGER_MS, SWEEP_ALPHA, join_rows, outline_rows, shape_of,
};

pub const MIN_ZOOM: f32 = 0.25;
pub const MAX_ZOOM: f32 = 16.0;
pub const SCRAMBLE_TICK_MS: f64 = 60.0;
pub const SCRAMBLE_ALPHA: f32 = 0.4;
pub const FLASH_MS: f64 = 1300.0;
pub const COPIED_LABEL_MS: f64 = 1500.0;
pub const TIP_FADE_MS: f64 = 120.0;

/// Key codes accepted by [`BlazeSite::on_key_down`].
pub mod key {
    pub const ESCAPE: u32 = 1;
    pub const ENTER: u32 = 2;
    pub const BACKSPACE: u32 = 3;
    pub const TAB: u32 = 4;
    pub const FIND_F: u32 = 10;
    pub const COPY_C: u32 = 11;
    pub const ALL_A: u32 = 12;
    pub const ZOOM_IN: u32 = 13;
    pub const ZOOM_OUT: u32 = 14;
    pub const ZOOM_RESET: u32 = 15;
    pub const THEME_L: u32 = 16;
}

/// Active pointer cursor requested by the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorKind {
    Default,
    Pointer,
    Text,
}

impl CursorKind {
    pub const fn css_name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Pointer => "pointer",
            Self::Text => "text",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct SelectionState {
    anchor: usize,
    focus: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct NoteFlash {
    rect: Rect,
    at_ms: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct FindBarGeometry {
    bar: Rect,
    prev_btn: Rect,
    next_btn: Rect,
    close_btn: Rect,
}

struct ActiveTip<A: Allocator + Copy> {
    key: OwnString<A>,
    layout: TipLayout<A>,
    /// Stage CSS position (column-local).
    x: f32,
    y: f32,
    above: bool,
    shown_at_ms: f64,
}

/// Complete pure-Blaze sans-IO front-page application.
pub struct BlazeSite<A: Allocator + Copy + 'static> {
    alloc: A,
    pub world: World<A>,
    pub reactive: ReactiveGraph<A>,
    pub sig_dark: Signal<bool>,
    pub sig_zoom: Signal<f32>,
    pub sig_hover_link: Signal<Option<u32>>,
    pub sig_hover_part: Signal<Option<u32>>,
    pub sig_hover_button: Signal<bool>,
    pub sig_find_open: Signal<bool>,

    pub model: PageModel<A>,
    pub handles: Option<SiteEcsHandles<A>>,
    pub table: SlugFontTable<A>,
    pub layout: PageLayout<A>,
    pub insts: OwnVec<SiteInst, A>,
    pub uniform: SiteUniform,

    /// Normalized atlas UV rect `[u0, v0, uw, vh]` for each of the 9 embedded SVGs.
    pub svg_uvs: [[f32; 4]; ImageName::COUNT],

    // Camera & viewport (CSS pixels)
    vp_w: f32,
    vp_h: f32,
    dpr: f32,
    col_w: f32,
    col_x0: f32,
    scroll_y: f32,
    pan_x: f32,
    layout_dirty: bool,
    model_loaded: bool,
    table_loaded: bool,

    // Interaction state
    now_ms: f64,
    pointer_sx: f32,
    pointer_sy: f32,
    dragging: bool,
    press_button: bool,
    selection: Option<SelectionState>,
    copied_range: Option<CharRange>,
    copied_at_ms: f64,
    copied_label_until_ms: f64,
    focus_link: Option<u32>,
    flash: Option<NoteFlash>,
    active_tip: Option<ActiveTip<A>>,
    cursor: CursorKind,

    // Click counting for double/triple-click selection
    last_click_ms: f64,
    last_click_x: f32,
    last_click_y: f32,
    click_count: u8,

    // Native WebGPU Cmd+F Find Bar state
    find_query: OwnString<A>,
    find_matches: OwnVec<CharRange, A>,
    find_current: usize,
    find_hover_btn: u8, // 0 none, 1 prev, 2 next, 3 close
    zoom_pill_hover: bool,

    // One-shot host side-effect queues (sans-IO!)
    pending_copy: OwnString<A>,
    pending_nav: OwnString<A>,

    // Telemetry
    pub frame_count: u64,
    pub last_glyph_count: u32,
}

impl<A: Allocator + Copy + 'static> BlazeSite<A> {
    pub fn new_in(alloc: A) -> Result<Self, SiteError> {
        let mut reactive = ReactiveGraph::new_in(alloc);
        let sig_dark = reactive.signal(true)?;
        let sig_zoom = reactive.signal(1.0f32)?;
        let sig_hover_link = reactive.signal::<Option<u32>>(None)?;
        let sig_hover_part = reactive.signal::<Option<u32>>(None)?;
        let sig_hover_button = reactive.signal(false)?;
        let sig_find_open = reactive.signal(false)?;

        Ok(Self {
            alloc,
            world: World::new_in(alloc)?,
            reactive,
            sig_dark,
            sig_zoom,
            sig_hover_link,
            sig_hover_part,
            sig_hover_button,
            sig_find_open,
            model: PageModel::empty_in(alloc),
            handles: None,
            table: SlugFontTable::empty_in(alloc),
            layout: PageLayout::empty_in(alloc),
            insts: OwnVec::new_in(alloc),
            uniform: SiteUniform::default(),
            svg_uvs: [[0.0; 4]; ImageName::COUNT],
            vp_w: 1440.0,
            vp_h: 900.0,
            dpr: 1.0,
            col_w: MAX_COL_WIDTH,
            col_x0: (1440.0 - MAX_COL_WIDTH) * 0.5,
            scroll_y: 0.0,
            pan_x: 0.0,
            layout_dirty: true,
            model_loaded: false,
            table_loaded: false,
            now_ms: 0.0,
            pointer_sx: -1000.0,
            pointer_sy: -1000.0,
            dragging: false,
            press_button: false,
            selection: None,
            copied_range: None,
            copied_at_ms: -10_000.0,
            copied_label_until_ms: -10_000.0,
            focus_link: None,
            flash: None,
            active_tip: None,
            cursor: CursorKind::Default,
            last_click_ms: -10_000.0,
            last_click_x: 0.0,
            last_click_y: 0.0,
            click_count: 0,
            find_query: OwnString::new_in(alloc),
            find_matches: OwnVec::new_in(alloc),
            find_current: 0,
            find_hover_btn: 0,
            zoom_pill_hover: false,
            pending_copy: OwnString::new_in(alloc),
            pending_nav: OwnString::new_in(alloc),
            frame_count: 0,
            last_glyph_count: 0,
        })
    }

    /// Decode a `BZG1` Slug binary font table (`text.<hash>.bin`) into `den` GPU storage buffers.
    pub fn load_font_table(&mut self, bytes: &[u8]) -> Result<(), SiteError> {
        self.table = SlugFontTable::decode_bzg1(bytes, self.alloc)?;
        self.table_loaded = true;
        self.layout_dirty = true;
        Ok(())
    }

    /// Parse `page.<hash>.json` and mount its entities + `ChildOf` + `DontFragment` tags into ECS.
    pub fn load_page_json(&mut self, bytes: &[u8]) -> Result<(), SiteError> {
        let model = PageModel::parse_json(bytes, self.alloc)?;
        self.world = World::new_in(self.alloc)?;
        let handles = model.mount_into_ecs(&mut self.world, self.alloc)?;
        self.model = model;
        self.handles = Some(handles);
        self.model_loaded = true;
        self.layout_dirty = true;
        self.active_tip = None;
        self.selection = None;
        self.find_matches.clear();
        Ok(())
    }

    pub fn set_dark(&mut self, dark: bool) -> Result<bool, SiteError> {
        Ok(self.reactive.set(self.sig_dark, dark)?)
    }

    pub fn is_dark(&self) -> bool {
        self.reactive.get(self.sig_dark).copied().unwrap_or(true)
    }

    /// Project the live `blaze_ecs::World` scene graph into semantic HTML (fallback when WebGPU
    /// is unavailable or `?renderer=html`). Returns the number of projected ECS entities.
    pub fn project_html(&self, out: &mut OwnString<A>) -> Result<u32, SiteError> {
        let Some(handles) = &self.handles else {
            out.clear();
            return Ok(0);
        };
        crate::html::project_ecs_html(&self.world, handles, &self.model, self.is_dark(), out, self.alloc)
    }

    /// Render a `1200×630` Open Graph RGBA8 image strictly from the live `blaze_ecs::World`
    /// (`ecs_query!` + SwiftUI-style `blaze_ui::ViewScope` + `LayoutEngine` + Rust GPU `slug_site_fs`).
    pub fn render_og_rgba(&mut self, rgba_out: &mut OwnVec<u8, A>) -> Result<crate::og::OgStats, SiteError> {
        if !self.model_loaded {
            rgba_out.clear();
            return Ok(crate::og::OgStats::default());
        }
        let dark = self.is_dark();
        crate::og::render_og_card_rgba(
            &mut self.world,
            &self.table,
            dark,
            rgba_out,
            self.alloc,
        )
    }

    pub fn theme(&self) -> SiteTheme {
        if self.is_dark() { SiteTheme::dark() } else { SiteTheme::light() }
    }

    pub fn zoom(&self) -> f32 {
        self.reactive.get(self.sig_zoom).copied().unwrap_or(1.0)
    }

    pub fn scroll_y(&self) -> f32 {
        self.scroll_y
    }

    pub fn pan_x(&self) -> f32 {
        self.pan_x
    }

    pub fn cursor(&self) -> CursorKind {
        self.cursor
    }

    pub fn find_open(&self) -> bool {
        self.reactive.get(self.sig_find_open).copied().unwrap_or(false)
    }

    pub fn find_query(&self) -> &str {
        self.find_query.as_str()
    }

    pub fn find_match_count(&self) -> usize {
        self.find_matches.len()
    }

    pub fn find_current_index(&self) -> usize {
        self.find_current
    }

    pub fn set_svg_uv(&mut self, img_idx: usize, u0: f32, v0: f32, uw: f32, vh: f32) {
        if let Some(slot) = self.svg_uvs.get_mut(img_idx) {
            *slot = [u0, v0, uw, vh];
        }
    }

    /// Take pending clipboard text (sans-IO host pulls and writes to `navigator.clipboard`).
    pub fn take_pending_copy(&mut self, out: &mut OwnString<A>) -> Result<bool, SiteError> {
        out.clear();
        if self.pending_copy.is_empty() {
            return Ok(false);
        }
        out.try_push_str(self.pending_copy.as_str())?;
        self.pending_copy.clear();
        Ok(true)
    }

    /// Take pending external URL navigation (sans-IO host pulls and opens URL).
    pub fn take_pending_nav(&mut self, out: &mut OwnString<A>) -> Result<bool, SiteError> {
        out.clear();
        if self.pending_nav.is_empty() {
            return Ok(false);
        }
        out.try_push_str(self.pending_nav.as_str())?;
        self.pending_nav.clear();
        Ok(true)
    }

    // -------------------------------------------------------------------------
    // Camera & Trackpad Pinch-to-Zoom Transforms
    // -------------------------------------------------------------------------

    #[inline]
    pub fn screen_to_page(&self, sx: f32, sy: f32) -> (f32, f32) {
        let z = self.zoom();
        let px = (sx - self.vp_w * 0.5) / z + self.vp_w * 0.5 - self.col_x0 + self.pan_x;
        let py = sy / z + self.scroll_y;
        (px, py)
    }

    #[inline]
    pub fn page_to_screen(&self, px: f32, py: f32) -> (f32, f32) {
        let z = self.zoom();
        let sx = (px + self.col_x0 - self.pan_x - self.vp_w * 0.5) * z + self.vp_w * 0.5;
        let sy = (py - self.scroll_y) * z;
        (sx, sy)
    }

    fn max_scroll(&self, z: f32) -> f32 {
        (self.layout.height - self.vp_h / z).max(0.0)
    }

    fn max_pan(&self, z: f32) -> f32 {
        if z <= 1.001 {
            return 0.0;
        }
        let half_col = self.col_w * 0.5;
        let half_vis = (self.vp_w * 0.5) / z;
        (half_col - half_vis + 80.0).max(self.vp_w * 0.25)
    }

    fn clamp_camera(&mut self) {
        let z = self.zoom();
        self.scroll_y = self.scroll_y.clamp(0.0, self.max_scroll(z));
        let mp = self.max_pan(z);
        if mp <= 0.0 {
            self.pan_x = 0.0;
        } else {
            self.pan_x = self.pan_x.clamp(-mp, mp);
        }
    }

    /// Cursor-anchored trackpad pinch-to-zoom (`factor > 1` zooms in, `< 1` zooms out).
    pub fn on_pinch(&mut self, sx: f32, sy: f32, factor: f32) -> Result<bool, SiteError> {
        if !factor.is_finite() || factor <= 0.0 {
            return Ok(false);
        }
        let (anchor_px, anchor_py) = self.screen_to_page(sx, sy);
        let old_z = self.zoom();
        let new_z = (old_z * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let changed = self.reactive.set(self.sig_zoom, new_z)?;
        if !changed {
            return Ok(false);
        }
        if new_z <= 1.001 {
            self.pan_x = 0.0;
        } else {
            self.pan_x = anchor_px + self.col_x0 - self.vp_w * 0.5 - (sx - self.vp_w * 0.5) / new_z;
        }
        self.scroll_y = anchor_py - sy / new_z;
        self.clamp_camera();
        Ok(true)
    }

    /// Two-finger trackpad scroll / horizontal pan when zoomed in.
    pub fn on_wheel(&mut self, dx: f32, dy: f32) {
        let z = self.zoom();
        self.scroll_y += dy / z;
        if z > 1.001 {
            self.pan_x += dx / z;
        } else {
            self.pan_x = 0.0;
        }
        self.clamp_camera();
    }

    /// Reset zoom to `100%` and re-center column cleanly.
    pub fn reset_zoom(&mut self) -> Result<bool, SiteError> {
        let changed = self.reactive.set(self.sig_zoom, 1.0)?;
        self.pan_x = 0.0;
        self.clamp_camera();
        Ok(changed)
    }

    /// Zoom around viewport center (used for `Cmd+=` / `Cmd+-`).
    pub fn zoom_center(&mut self, factor: f32) -> Result<bool, SiteError> {
        self.on_pinch(self.vp_w * 0.5, self.vp_h * 0.5, factor)
    }

    // -------------------------------------------------------------------------
    // Viewport-Pinned HUD Geometry (Find Bar & Zoom Pill)
    // -------------------------------------------------------------------------

    fn find_bar_geom(&self) -> FindBarGeometry {
        let w = 310.0f32.min((self.vp_w - 28.0).max(200.0));
        let h = 40.0;
        let x = (self.vp_w - 16.0 - w).max(12.0);
        let y = 14.0;
        let bar = Rect::new(x, y, w, h);
        let btn_s = 26.0;
        let by = y + (h - btn_s) * 0.5;
        let close_btn = Rect::new(x + w - 7.0 - btn_s, by, btn_s, btn_s);
        let next_btn = Rect::new(close_btn.x - 2.0 - btn_s, by, btn_s, btn_s);
        let prev_btn = Rect::new(next_btn.x - 2.0 - btn_s, by, btn_s, btn_s);
        FindBarGeometry { bar, prev_btn, next_btn, close_btn }
    }

    fn zoom_pill_rect(&self) -> Option<Rect> {
        if (self.zoom() - 1.0).abs() <= 0.01 {
            return None;
        }
        let w = 124.0;
        let h = 30.0;
        Some(Rect::new((self.vp_w - 16.0 - w).max(12.0), (self.vp_h - 16.0 - h).max(12.0), w, h))
    }

    // -------------------------------------------------------------------------
    // ECS DontFragment Tag Sync (Zero Archetype Migrations!)
    // -------------------------------------------------------------------------

    fn sync_tag_hovered(&mut self, ent: Entity, active: bool) -> Result<(), SiteError> {
        if active {
            self.world.add_tag_typed::<SiteHovered>(ent)?;
        } else {
            let _ = self.world.remove_tag_typed::<SiteHovered>(ent)?;
        }
        Ok(())
    }

    fn set_hover_link(&mut self, next: Option<u32>) -> Result<bool, SiteError> {
        let prev = self.reactive.get(self.sig_hover_link).copied().unwrap_or(None);
        if prev == next {
            return Ok(false);
        }
        let _ = self.reactive.set(self.sig_hover_link, next)?;
        let prev_ent = prev.and_then(|id| self.handles.as_ref()?.link_entities.get(usize::try_from(id).ok()?).copied());
        let next_ent = next.and_then(|id| self.handles.as_ref()?.link_entities.get(usize::try_from(id).ok()?).copied());
        if let Some(e) = prev_ent {
            self.sync_tag_hovered(e, false)?;
        }
        if let Some(e) = next_ent {
            self.sync_tag_hovered(e, true)?;
        }
        Ok(true)
    }

    fn set_hover_part(&mut self, next: Option<u32>) -> Result<bool, SiteError> {
        let prev = self.reactive.get(self.sig_hover_part).copied().unwrap_or(None);
        if prev == next {
            return Ok(false);
        }
        let _ = self.reactive.set(self.sig_hover_part, next)?;
        let prev_ent = prev.and_then(|id| self.handles.as_ref()?.part_entities.get(usize::try_from(id).ok()?).copied());
        let next_ent = next.and_then(|id| self.handles.as_ref()?.part_entities.get(usize::try_from(id).ok()?).copied());
        if let Some(e) = prev_ent {
            self.sync_tag_hovered(e, false)?;
        }
        if let Some(e) = next_ent {
            self.sync_tag_hovered(e, true)?;
        }
        Ok(true)
    }

    fn set_hover_button(&mut self, next: bool) -> Result<bool, SiteError> {
        let prev = self.reactive.get(self.sig_hover_button).copied().unwrap_or(false);
        if prev == next {
            return Ok(false);
        }
        let _ = self.reactive.set(self.sig_hover_button, next)?;
        if let Some(ent) = self.handles.as_ref().map(|h| h.copy_button) {
            self.sync_tag_hovered(ent, next)?;
        }
        Ok(true)
    }

    fn set_find_open(&mut self, open: bool) -> Result<bool, SiteError> {
        let changed = self.reactive.set(self.sig_find_open, open)?;
        if changed {
            if let Some(ent) = self.handles.as_ref().map(|h| h.find_bar) {
                if open {
                    self.world.add_tag_typed::<SiteFocused>(ent)?;
                } else {
                    let _ = self.world.remove_tag_typed::<SiteFocused>(ent)?;
                }
            }
        }
        Ok(changed)
    }

    // -------------------------------------------------------------------------
    // Hover Card Management
    // -------------------------------------------------------------------------

    fn update_hover_tip(&mut self, key: Option<&str>, anchor: Option<Rect>) -> Result<(), SiteError> {
        let Some(k) = key else {
            self.active_tip = None;
            return Ok(());
        };
        let Some(a) = anchor else {
            self.active_tip = None;
            return Ok(());
        };
        if let Some(cur) = &self.active_tip {
            if cur.key.as_str() == k {
                return Ok(());
            }
        }
        let Some(tip_layout) = layout_tip(&self.model, k, self.col_w, &self.table, self.alloc)? else {
            self.active_tip = None;
            return Ok(());
        };
        let mut x = a.x + (a.w - tip_layout.w) * 0.5;
        x = x.clamp(8.0, (self.col_w - tip_layout.w - 8.0).max(8.0));
        let gap = 8.0;
        let vis_top = self.scroll_y;
        let above_y = a.y - tip_layout.h - gap;
        let (y, above) = if above_y >= vis_top + 8.0 {
            (above_y, true)
        } else {
            (a.y + a.h + gap, false)
        };
        let mut key_copy = OwnString::new_in(self.alloc);
        key_copy.try_push_str(k)?;
        self.active_tip = Some(ActiveTip {
            key: key_copy,
            layout: tip_layout,
            x,
            y,
            above,
            shown_at_ms: self.now_ms,
        });
        Ok(())
    }

    // -------------------------------------------------------------------------
    // Pointer & Keyboard Input Handlers
    // -------------------------------------------------------------------------

    fn hit_link(&self, px: f32, py: f32) -> Option<(u32, Rect)> {
        for (lid, rects) in self.layout.links.as_slice().iter().enumerate() {
            for &r in rects.as_slice() {
                if point_in_rect(px, py, r) {
                    return Some((u32::try_from(lid).unwrap_or(0), r));
                }
            }
        }
        None
    }

    fn hit_part(&self, px: f32, py: f32) -> Option<(u32, Rect)> {
        for (pid, slot) in self.layout.parts.as_slice().iter().enumerate() {
            if let Some(r) = slot {
                let expanded = Rect::new(r.x - 3.0, r.y - 3.0, r.w + 6.0, r.h + 6.0);
                if point_in_rect(px, py, expanded) {
                    return Some((u32::try_from(pid).unwrap_or(0), *r));
                }
            }
        }
        None
    }

    pub fn on_pointer_move(&mut self, sx: f32, sy: f32, now_ms: f64) -> Result<(), SiteError> {
        self.now_ms = now_ms;
        self.pointer_sx = sx;
        self.pointer_sy = sy;
        self.ensure_layout()?;

        // Check viewport-pinned HUD first
        self.find_hover_btn = 0;
        self.zoom_pill_hover = false;
        if self.find_open() {
            let g = self.find_bar_geom();
            if point_in_rect(sx, sy, g.bar) {
                if point_in_rect(sx, sy, g.prev_btn) {
                    self.find_hover_btn = 1;
                    self.cursor = CursorKind::Pointer;
                } else if point_in_rect(sx, sy, g.next_btn) {
                    self.find_hover_btn = 2;
                    self.cursor = CursorKind::Pointer;
                } else if point_in_rect(sx, sy, g.close_btn) {
                    self.find_hover_btn = 3;
                    self.cursor = CursorKind::Pointer;
                } else {
                    self.cursor = CursorKind::Text;
                }
                return Ok(());
            }
        }
        if let Some(zr) = self.zoom_pill_rect() {
            if point_in_rect(sx, sy, zr) {
                self.zoom_pill_hover = true;
                self.cursor = CursorKind::Pointer;
                return Ok(());
            }
        }

        let (px, py) = self.screen_to_page(sx, sy);
        if self.dragging {
            let idx = char_at_point(&self.layout, px, py);
            if let Some(sel) = &mut self.selection {
                sel.focus = idx;
            }
            self.cursor = CursorKind::Text;
            return Ok(());
        }

        let over_btn = self.layout.button.is_some_and(|r| point_in_rect(px, py, r));
        let _ = self.set_hover_button(over_btn)?;
        if over_btn {
            let _ = self.set_hover_link(None)?;
            let _ = self.set_hover_part(None)?;
            self.active_tip = None;
            self.cursor = CursorKind::Pointer;
            return Ok(());
        }

        if let Some((pid, pr)) = self.hit_part(px, py) {
            let _ = self.set_hover_link(None)?;
            let _ = self.set_hover_part(Some(pid))?;
            let mut key_buf = OwnString::new_in(self.alloc);
            let _ = write!(&mut key_buf, "part:{pid}");
            if self.model.find_tip(key_buf.as_str()).is_none() {
                key_buf.clear();
                if let Some(k) = self.command_part_tip_key(pid) {
                    key_buf.try_push_str(k)?;
                }
            }
            self.update_hover_tip(
                if key_buf.is_empty() { None } else { Some(key_buf.as_str()) },
                Some(pr),
            )?;
            self.cursor = CursorKind::Default;
            return Ok(());
        }
        let _ = self.set_hover_part(None)?;

        if let Some((lid, lr)) = self.hit_link(px, py) {
            let _ = self.set_hover_link(Some(lid))?;
            let mut key_buf = OwnString::new_in(self.alloc);
            if let Some(link) = self.model.links.get(usize::try_from(lid).unwrap_or(usize::MAX)) {
                if let Some(tip) = &link.tip {
                    key_buf.try_push_str(tip.as_str())?;
                }
            }
            self.update_hover_tip(
                if key_buf.is_empty() { None } else { Some(key_buf.as_str()) },
                Some(lr),
            )?;
            self.cursor = CursorKind::Pointer;
            return Ok(());
        }
        let _ = self.set_hover_link(None)?;
        self.active_tip = None;

        // Text I-beam cursor when hovering over any text char box
        let over_text = self.layout.chars.as_slice().iter().any(|c| {
            px >= c.x0 - 4.0 && px <= c.x1 + 4.0 && py >= c.top && py <= c.bottom
        });
        self.cursor = if over_text { CursorKind::Text } else { CursorKind::Default };
        Ok(())
    }

    pub fn on_pointer_down(&mut self, sx: f32, sy: f32, now_ms: f64) -> Result<(), SiteError> {
        self.now_ms = now_ms;
        self.pointer_sx = sx;
        self.pointer_sy = sy;
        self.ensure_layout()?;

        // Find bar buttons
        if self.find_open() {
            let g = self.find_bar_geom();
            if point_in_rect(sx, sy, g.bar) {
                if point_in_rect(sx, sy, g.prev_btn) {
                    self.step_find(-1);
                } else if point_in_rect(sx, sy, g.next_btn) {
                    self.step_find(1);
                } else if point_in_rect(sx, sy, g.close_btn) {
                    let _ = self.set_find_open(false)?;
                }
                return Ok(());
            }
        }
        // Zoom pill click -> reset zoom to 100%
        if let Some(zr) = self.zoom_pill_rect() {
            if point_in_rect(sx, sy, zr) {
                let _ = self.reset_zoom()?;
                return Ok(());
            }
        }

        let (px, py) = self.screen_to_page(sx, sy);

        // Copy button press
        if self.layout.button.is_some_and(|r| point_in_rect(px, py, r)) {
            self.press_button = true;
            if let Some(ent) = self.handles.as_ref().map(|h| h.copy_button) {
                self.world.add_tag_typed::<SitePressed>(ent)?;
            }
            return Ok(());
        }

        // Link click (if not dragging)
        if let Some((lid, _)) = self.hit_link(px, py) {
            self.focus_link = Some(lid);
            self.selection = None;
            let mut nav_href = OwnString::new_in(self.alloc);
            let mut note_id = OwnString::new_in(self.alloc);
            if let Some(link) = self.model.links.get(usize::try_from(lid).unwrap_or(usize::MAX)) {
                if let Some(n) = &link.note {
                    note_id.try_push_str(n.as_str())?;
                } else if !link.href.is_empty() {
                    nav_href.try_push_str(link.href.as_str())?;
                }
            }
            if !note_id.is_empty() {
                if let Some(nr) = self.layout.note_rect(note_id.as_str()) {
                    let z = self.zoom();
                    self.scroll_y = nr.y + nr.h * 0.5 - (self.vp_h * 0.5) / z;
                    self.clamp_camera();
                    self.flash = Some(NoteFlash { rect: nr, at_ms: now_ms });
                }
            } else if !nav_href.is_empty() {
                self.pending_nav.clear();
                self.pending_nav.try_push_str(nav_href.as_str())?;
            }
            return Ok(());
        }

        // Multi-click & drag text selection
        let dt = now_ms - self.last_click_ms;
        let dist = (sx - self.last_click_x).abs() + (sy - self.last_click_y).abs();
        if dt < 420.0 && dist < 8.0 {
            self.click_count = self.click_count.saturating_add(1).min(3);
        } else {
            self.click_count = 1;
        }
        self.last_click_ms = now_ms;
        self.last_click_x = sx;
        self.last_click_y = sy;
        self.focus_link = None;

        let idx = char_at_point(&self.layout, px, py);
        match self.click_count {
            2 => {
                let r = word_range(&self.layout, idx);
                self.selection = Some(SelectionState { anchor: r.from, focus: r.to });
                self.dragging = false;
            }
            3 => {
                let r = block_item_range(&self.layout, idx);
                self.selection = Some(SelectionState { anchor: r.from, focus: r.to });
                self.dragging = false;
            }
            _ => {
                self.selection = Some(SelectionState { anchor: idx, focus: idx });
                self.dragging = true;
            }
        }
        Ok(())
    }

    pub fn on_pointer_up(&mut self, sx: f32, sy: f32, now_ms: f64) -> Result<(), SiteError> {
        self.now_ms = now_ms;
        self.pointer_sx = sx;
        self.pointer_sy = sy;
        let (px, py) = self.screen_to_page(sx, sy);

        if self.press_button {
            self.press_button = false;
            if let Some(ent) = self.handles.as_ref().map(|h| h.copy_button) {
                let _ = self.world.remove_tag_typed::<SitePressed>(ent)?;
            }
            if self.layout.button.is_some_and(|r| point_in_rect(px, py, r)) {
                self.trigger_copy_command(now_ms)?;
            }
        }
        if self.dragging {
            self.dragging = false;
            if let Some(sel) = self.selection {
                if sel.anchor == sel.focus {
                    self.selection = None;
                }
            }
        }
        Ok(())
    }

    fn trigger_copy_command(&mut self, now_ms: f64) -> Result<(), SiteError> {
        self.pending_copy.clear();
        for blk in self.model.blocks.as_slice() {
            if let Block::Command { copy, .. } = blk {
                self.pending_copy.try_push_str(copy.as_str())?;
                break;
            }
        }
        self.copied_label_until_ms = now_ms + COPIED_LABEL_MS;
        // Also select & pulse the command characters cleanly
        if let Some(cmd_r) = self.layout.command {
            let mut from = usize::MAX;
            let mut to = 0usize;
            for (i, c) in self.layout.chars.as_slice().iter().enumerate() {
                if c.top >= cmd_r.y && c.bottom <= cmd_r.y + cmd_r.h {
                    from = from.min(i);
                    to = to.max(i + 1);
                }
            }
            if from < to {
                self.selection = Some(SelectionState { anchor: from, focus: to });
                self.copied_range = Some(CharRange { from, to });
                self.copied_at_ms = now_ms;
            }
        }
        Ok(())
    }

    /// Handle keyboard shortcuts and Find Bar navigation. Returns `true` if event was handled.
    pub fn on_key_down(&mut self, code: u32, shift: bool, mod_key: bool, now_ms: f64) -> Result<bool, SiteError> {
        self.now_ms = now_ms;
        self.ensure_layout()?;

        if mod_key && code == key::FIND_F {
            let _ = self.set_find_open(true)?;
            self.recompute_find(true)?;
            return Ok(true);
        }
        if mod_key && code == key::ZOOM_IN {
            let _ = self.zoom_center(1.25)?;
            return Ok(true);
        }
        if mod_key && code == key::ZOOM_OUT {
            let _ = self.zoom_center(0.8)?;
            return Ok(true);
        }
        if mod_key && code == key::ZOOM_RESET {
            let _ = self.reset_zoom()?;
            return Ok(true);
        }
        if mod_key && shift && code == key::THEME_L {
            let dark = !self.is_dark();
            let _ = self.set_dark(dark)?;
            return Ok(true);
        }
        if mod_key && code == key::ALL_A && !self.find_open() {
            if !self.layout.chars.is_empty() {
                self.selection = Some(SelectionState { anchor: 0, focus: self.layout.chars.len() });
            }
            return Ok(true);
        }
        if mod_key && code == key::COPY_C {
            if let Some(sel) = self.selection {
                let lo = sel.anchor.min(sel.focus);
                let hi = sel.anchor.max(sel.focus);
                if lo < hi {
                    let mut buf = OwnString::new_in(self.alloc);
                    copy_text(&self.layout, lo, hi, &mut buf)?;
                    self.pending_copy = buf;
                    self.copied_range = Some(CharRange { from: lo, to: hi });
                    self.copied_at_ms = now_ms;
                    return Ok(true);
                }
            }
        }

        if self.find_open() {
            match code {
                key::ESCAPE => {
                    let _ = self.set_find_open(false)?;
                    return Ok(true);
                }
                key::ENTER => {
                    self.step_find(if shift { -1 } else { 1 });
                    return Ok(true);
                }
                key::BACKSPACE => {
                    let _ = self.find_query.pop();
                    self.recompute_find(true)?;
                    return Ok(true);
                }
                _ => {}
            }
        } else if code == key::ESCAPE {
            self.selection = None;
            self.active_tip = None;
            return Ok(true);
        }

        Ok(false)
    }

    /// Feed a printable character into the active native WebGPU Cmd+F Find Bar.
    pub fn on_text_char(&mut self, ch: char, now_ms: f64) -> Result<bool, SiteError> {
        self.now_ms = now_ms;
        if !self.find_open() || ch.is_control() {
            return Ok(false);
        }
        if self.find_query.len() < 64 {
            self.find_query.try_push(ch)?;
            self.recompute_find(true)?;
        }
        Ok(true)
    }

    fn recompute_find(&mut self, reveal: bool) -> Result<(), SiteError> {
        self.find_matches = find_matches(&self.layout, self.find_query.as_str(), self.alloc)?;
        self.find_current = 0;
        if reveal && !self.find_matches.is_empty() {
            self.reveal_current_match();
        }
        Ok(())
    }

    fn step_find(&mut self, delta: i32) {
        let n = self.find_matches.len();
        if n == 0 {
            return;
        }
        if delta >= 0 {
            self.find_current = (self.find_current + 1) % n;
        } else if self.find_current == 0 {
            self.find_current = n - 1;
        } else {
            self.find_current -= 1;
        }
        self.reveal_current_match();
    }

    fn reveal_current_match(&mut self) {
        let Some(m) = self.find_matches.get(self.find_current) else { return };
        let Some(c) = self.layout.chars.get(m.from) else { return };
        let z = self.zoom();
        let vis_top = self.scroll_y;
        let vis_h = self.vp_h / z;
        if c.top < vis_top + 60.0 || c.bottom > vis_top + vis_h - 60.0 {
            self.scroll_y = (c.top + c.bottom) * 0.5 - vis_h * 0.4;
            self.clamp_camera();
        }
    }

    fn command_part_tip_key(&self, pid: u32) -> Option<&str> {
        let idx = usize::try_from(pid).ok()?;
        for blk in self.model.blocks.as_slice() {
            if let Block::Command { parts, .. } = blk {
                return parts.get(idx).map(|s| s.as_str());
            }
        }
        None
    }

    // -------------------------------------------------------------------------
    // Layout & Single-Pass `den` Slug + SDF Instance Emitter
    // -------------------------------------------------------------------------

    fn ensure_layout(&mut self) -> Result<(), SiteError> {
        if !self.layout_dirty || !self.model_loaded {
            return Ok(());
        }
        self.col_w = self.vp_w.min(MAX_COL_WIDTH).max(200.0);
        self.col_x0 = (self.vp_w - self.col_w) * 0.5;
        let win = Viewport { width: self.vp_w, height: self.vp_h };
        self.layout = layout_page(&self.model, self.col_w, &self.table, win, self.alloc)?;
        self.layout_dirty = false;
        self.clamp_camera();
        if self.find_open() && !self.find_query.is_empty() {
            self.find_matches = find_matches(&self.layout, self.find_query.as_str(), self.alloc)?;
            if self.find_current >= self.find_matches.len() {
                self.find_current = 0;
            }
        }
        Ok(())
    }

    /// Pack the entire frame (page + `den` Slug vector glyphs + hover card + native WebGPU Find Bar
    /// + Zoom HUD pill) into `self.insts` and `self.uniform`.
    ///
    /// Returns `Ok(true)` if an animation needs another frame (copy pulse, footnote flash, hover
    /// card fade, scrambled `ucki` glyphs, or blinking find-bar caret), `Ok(false)` when idle.
    pub fn render_frame(
        &mut self,
        vp_w: f32,
        vp_h: f32,
        dpr: f32,
        now_ms: f64,
    ) -> Result<bool, SiteError> {
        self.now_ms = now_ms;
        let safe_w = vp_w.max(64.0);
        let safe_h = vp_h.max(64.0);
        let safe_dpr = dpr.clamp(0.5, 4.0);
        if (safe_w - self.vp_w).abs() > 0.5 || (safe_h - self.vp_h).abs() > 0.5 {
            self.vp_w = safe_w;
            self.vp_h = safe_h;
            self.layout_dirty = true;
        }
        self.dpr = safe_dpr;
        self.ensure_layout()?;

        self.insts.clear();
        self.last_glyph_count = 0;
        self.frame_count = self.frame_count.saturating_add(1);

        let theme = self.theme();
        let zoom = self.zoom();
        let page_scale = zoom * safe_dpr;
        let vp_w_dev = safe_w * safe_dpr;
        let vp_h_dev = safe_h * safe_dpr;
        let mut wants_frame = false;

        let hover_link = self.reactive.get(self.sig_hover_link).copied().unwrap_or(None);
        let hover_part = self.reactive.get(self.sig_hover_part).copied().unwrap_or(None);
        let hover_button = self.reactive.get(self.sig_hover_button).copied().unwrap_or(false);

        // Compute page-to-device helper coordinates
        let col_x0 = self.col_x0;
        let pan_x = self.pan_x;
        let scroll_y = self.scroll_y;
        let p2d_x = |px: f32| -> f32 {
            ((px + col_x0 - pan_x - safe_w * 0.5) * zoom + safe_w * 0.5) * safe_dpr
        };
        let p2d_y = |py: f32| -> f32 { (py - scroll_y) * page_scale };

        // 1. Page shapes (base shapes -> hovered part plate -> hovered link plate)
        for pass in 0u8..3u8 {
            for idx in 0..self.layout.shapes.len() {
                let Some(&s) = self.layout.shapes.get(idx) else { continue };
                let emit = match pass {
                    0 => s.kind != ShapeKind::PartPlate && s.kind != ShapeKind::LinkPlate,
                    1 => s.kind == ShapeKind::PartPlate && s.part == hover_part && hover_part.is_some(),
                    _ => s.kind == ShapeKind::LinkPlate && s.link == hover_link && hover_link.is_some(),
                };
                if !emit {
                    continue;
                }
                let dy = p2d_y(s.rect.y);
                let dh = s.rect.h * page_scale;
                if dy + dh < -32.0 || dy > vp_h_dev + 32.0 {
                    continue;
                }
                self.emit_shape(s, 0.0, 0.0, 1.0, page_scale, theme, hover_link, hover_button, &p2d_x, &p2d_y)?;
            }
        }

        // 2a. Find matches highlights (warm orange `--fire`, 0.55 current / 0.22 other)
        if self.find_open() && !self.find_matches.is_empty() {
            let mut row_buf: OwnVec<Row, A> = OwnVec::new_in(self.alloc);
            for mi in 0..self.find_matches.len() {
                let Some(&m) = self.find_matches.get(mi) else { continue };
                row_buf.clear();
                let hi = m.to.min(self.layout.chars.len());
                for ci in m.from..hi {
                    let Some(c) = self.layout.chars.get(ci) else { continue };
                    let lx = p2d_x(c.x0);
                    let rx = p2d_x(c.x1);
                    let ty = gpu::math::round(p2d_y(c.top));
                    let by = gpu::math::round(p2d_y(c.bottom));
                    let mut merged = false;
                    for r in row_buf.as_mut_slice() {
                        if (r.top - ty).abs() < 2.0 {
                            r.left = r.left.min(lx);
                            r.right = r.right.max(rx);
                            merged = true;
                            break;
                        }
                    }
                    if !merged {
                        row_buf
                            .try_push(Row { left: lx, top: ty, right: rx, bottom: by })
                            .map_err(|e| SiteError::Alloc(e.why))?;
                    }
                }
                let alpha = if mi == self.find_current { 0.55 } else { 0.22 };
                let fill = color_vec(theme.fire, alpha);
                let rad = 3.0 * page_scale;
                for &r in row_buf.as_slice() {
                    self.push_inst(SiteInst {
                        rect: vec4(r.left - page_scale, r.top, r.right - r.left + 2.0 * page_scale, r.bottom - r.top),
                        a: vec4(rad, rad, rad, rad),
                        color: fill,
                        fx: vec4(site_kind::PLATE, 0.0, 0.0, 0.0),
                        band_tf: ZERO_V4,
                        gdata: ZERO_U4,
                    })?;
                }
            }
        }

        // 2b. Liquid selection rows + copy-pulse light sweep + concave fillets
        let mut frags: OwnVec<Row, A> = OwnVec::new_in(self.alloc);
        if let Some(sel) = self.selection {
            let lo = sel.anchor.min(sel.focus);
            let hi = sel.anchor.max(sel.focus).min(self.layout.chars.len());
            for ci in lo..hi {
                let Some(c) = self.layout.chars.get(ci) else { continue };
                frags
                    .try_push(Row {
                        left: p2d_x(c.x0),
                        top: gpu::math::round(p2d_y(c.top)),
                        right: p2d_x(c.x1),
                        bottom: gpu::math::round(p2d_y(c.bottom)),
                    })
                    .map_err(|e| SiteError::Alloc(e.why))?;
            }
        }
        let mut sel_rows = join_rows(frags.as_slice(), 10.0 * page_scale, 1.0, self.alloc)?;
        let room = gpu::math::round(2.0 * page_scale);
        for r in sel_rows.as_mut_slice() {
            r.left -= room;
            r.right += room;
        }
        let ms_since_copy = f64_to_f32(now_ms - self.copied_at_ms);
        let pulse = shape_of(sel_rows.as_slice(), ms_since_copy, false);
        if pulse.live || (ms_since_copy >= 0.0 && ms_since_copy < PULSE_MS + 34.0) {
            wants_frame = true;
        }
        let sel_fill = color_vec(theme.selection, 1.0);
        let (radii, fillets) = outline_rows(sel_rows.as_slice(), 4.0 * page_scale, self.alloc)?;
        for (ri, &row) in sel_rows.as_slice().iter().enumerate() {
            let rad = radii.get(ri).copied().unwrap_or([0.0; 4]);
            let w = row.right - row.left;
            let h = row.bottom - row.top;
            self.push_inst(SiteInst {
                rect: vec4(row.left, row.top, w, h),
                a: vec4(rad[0], rad[1], rad[2], rad[3]),
                color: sel_fill,
                fx: vec4(site_kind::PLATE, 1.0, 0.0, 0.0),
                band_tf: ZERO_V4,
                gdata: ZERO_U4,
            })?;
            if pulse.live {
                let enter = ((ms_since_copy - usize_to_f32(ri) * STAGGER_MS) / ROW_IN_MS).clamp(0.0, 1.0);
                let lit = gpu::math::sin(PI * pulse.phase).max(0.0) * SWEEP_ALPHA * enter;
                self.push_inst(SiteInst {
                    rect: vec4(row.left, row.top, w, h),
                    a: vec4(rad[0], rad[1], rad[2], rad[3]),
                    color: vec4(1.0, 1.0, 1.0, lit),
                    fx: vec4(site_kind::LIGHT, 1.0, pulse.phase, 0.0),
                    band_tf: ZERO_V4,
                    gdata: ZERO_U4,
                })?;
            }
        }
        for &f in fillets.as_slice() {
            self.push_inst(SiteInst {
                rect: vec4(f.x, f.y, f.r, f.r),
                a: vec4(f.towards[0], f.towards[1], 0.0, 0.0),
                color: sel_fill,
                fx: vec4(site_kind::FILLET, 1.0, 0.0, 0.0),
                band_tf: ZERO_V4,
                gdata: ZERO_U4,
            })?;
        }

        // 3. Footnote jump flash plate
        if let Some(fl) = self.flash {
            let t = f64_to_f32((now_ms - fl.at_ms) / FLASH_MS);
            if t >= 0.0 && t < 1.0 {
                wants_frame = true;
                let a = 0.16 * if t <= 0.18 { 1.0 } else { 1.0 - ease_out((t - 0.18) / 0.82) };
                let r = 12.0 * page_scale;
                self.push_inst(SiteInst {
                    rect: vec4(
                        p2d_x(fl.rect.x - 6.0),
                        p2d_y(fl.rect.y - 6.0),
                        (fl.rect.w + 12.0) * page_scale,
                        (fl.rect.h + 12.0) * page_scale,
                    ),
                    a: vec4(r, r, r, r),
                    color: color_vec(theme.ink_1, a),
                    fx: vec4(site_kind::PLATE, 0.0, 0.0, 0.0),
                    band_tf: ZERO_V4,
                    gdata: ZERO_U4,
                })?;
            }
        }

        // 4. Inline & signature SVG pictures
        for idx in 0..self.layout.pics.len() {
            let Some(&p) = self.layout.pics.get(idx) else { continue };
            let dy = p2d_y(p.rect.y);
            let dh = p.rect.h * page_scale;
            if dy + dh < -32.0 || dy > vp_h_dev + 32.0 {
                continue;
            }
            self.emit_pic(p, 0.0, 0.0, 1.0, page_scale, theme, &p2d_x, &p2d_y)?;
        }

        // 5. `den` Slug GPU Quadratic Bézier Outlines (razor-sharp at ANY trackpad zoom!)
        let btn_label: u8 = if now_ms < self.copied_label_until_ms {
            wants_frame = true;
            1
        } else {
            0
        };
        let active_copied = if pulse.live { self.copied_range } else { None };
        for idx in 0..self.layout.glyphs.len() {
            let Some(&g) = self.layout.glyphs.get(idx) else { continue };
            if g.button.is_some_and(|b| b != btn_label) {
                continue;
            }
            let dy = p2d_y(g.y);
            let sz_dev = g.size * page_scale;
            if dy + sz_dev * 1.5 < -48.0 || dy - sz_dev * 1.5 > vp_h_dev + 48.0 {
                continue;
            }
            let is_copied = match (active_copied, g.char_idx) {
                (Some(cr), Some(ci)) => {
                    let cu = usize::try_from(ci).unwrap_or(usize::MAX);
                    cu >= cr.from && cu < cr.to
                }
                _ => false,
            };
            let scrambled_on_screen = self.emit_slug_glyph(
                g, 0.0, 0.0, 1.0, page_scale, theme, hover_link, hover_button, is_copied, now_ms, &p2d_x, &p2d_y,
            )?;
            if scrambled_on_screen {
                wants_frame = true;
            }
        }

        // 6. Keyboard focus ring around focused link
        if let Some(fl) = self.focus_link {
            let link_idx = usize::try_from(fl).unwrap_or(usize::MAX);
            let n_rects = self.layout.links.get(link_idx).map_or(0, |r| r.len());
            let r = 4.0 * page_scale;
            for ri in 0..n_rects {
                let Some(b) = self.layout.links.get(link_idx).and_then(|v| v.get(ri)).copied() else {
                    continue;
                };
                self.push_inst(SiteInst {
                    rect: vec4(
                        p2d_x(b.x - 5.0),
                        p2d_y(b.y - 5.0),
                        (b.w + 10.0) * page_scale,
                        (b.h + 10.0) * page_scale,
                    ),
                    a: vec4(r, r, r, r),
                    color: color_vec(theme.ink_1, 1.0),
                    fx: vec4(site_kind::RING, 0.0, 2.0 * page_scale, 0.0),
                    band_tf: ZERO_V4,
                    gdata: ZERO_U4,
                })?;
            }
        }

        // 7. Floating hover card (`TipLayout`) with soft SDF drop shadow + Slug glyphs
        if self.active_tip.is_some() {
            let (tx, ty, above, shown_at, n_shapes, n_pics, n_glyphs, card_rect_opt) = match &self.active_tip {
                Some(t) => (
                    t.x,
                    t.y,
                    t.above,
                    t.shown_at_ms,
                    t.layout.shapes.len(),
                    t.layout.pics.len(),
                    t.layout.glyphs.len(),
                    t.layout.shapes.first().copied(),
                ),
                None => (0.0, 0.0, false, 0.0, 0, 0, 0, None),
            };
            let prog = f64_to_f32((now_ms - shown_at) / TIP_FADE_MS).clamp(0.0, 1.0);
            if prog < 1.0 {
                wants_frame = true;
            }
            let alpha = ease_out(prog);
            let slide = (1.0 - alpha) * if above { 4.0 } else { -4.0 };
            let dy_off = ty + slide;

            if let Some(card) = card_rect_opt {
                let r = (card.r + 12.0) * page_scale;
                self.push_inst(SiteInst {
                    rect: vec4(
                        p2d_x(tx + card.rect.x - 12.0),
                        p2d_y(dy_off + card.rect.y + 8.0 - 12.0),
                        (card.rect.w + 24.0) * page_scale,
                        (card.rect.h + 24.0) * page_scale,
                    ),
                    a: vec4(r, r, r, r),
                    color: vec4(0.0, 0.0, 0.0, 0.22 * alpha),
                    fx: vec4(site_kind::SHADOW, 0.0, 12.0 * page_scale, 0.0),
                    band_tf: ZERO_V4,
                    gdata: ZERO_U4,
                })?;
            }
            for i in 0..n_shapes {
                let s_opt = self.active_tip.as_ref().and_then(|t| t.layout.shapes.get(i).copied());
                if let Some(s) = s_opt {
                    self.emit_shape(s, tx, dy_off, alpha, page_scale, theme, hover_link, false, &p2d_x, &p2d_y)?;
                }
            }
            for i in 0..n_pics {
                let p_opt = self.active_tip.as_ref().and_then(|t| t.layout.pics.get(i).copied());
                if let Some(p) = p_opt {
                    self.emit_pic(p, tx, dy_off, alpha, page_scale, theme, &p2d_x, &p2d_y)?;
                }
            }
            for i in 0..n_glyphs {
                let g_opt = self.active_tip.as_ref().and_then(|t| t.layout.glyphs.get(i).copied());
                if let Some(g) = g_opt {
                    let _ = self.emit_slug_glyph(
                        g, tx, dy_off, alpha, page_scale, theme, hover_link, false, false, now_ms, &p2d_x, &p2d_y,
                    )?;
                }
            }
        }

        // 8. Native WebGPU `Cmd+F` Find Bar (viewport-pinned at 1.0 * dpr, Slug vector text!)
        if self.find_open() {
            wants_frame = true; // blinking caret
            self.emit_find_bar(safe_dpr, theme, now_ms)?;
        }

        // 9. Zoom HUD Pill (bottom-right when zoom != 100%, viewport-pinned, click to reset!)
        if let Some(zr) = self.zoom_pill_rect() {
            self.emit_zoom_pill(zr, safe_dpr, theme)?;
        }

        // Pack `SiteUniform` (64 bytes, matches `gpu::slug::SiteUniform` exactly)
        let glow = if pulse.live {
            gpu::math::pow(gpu::math::sin(PI * (pulse.phase * 1.15).min(1.0)).max(0.0), 0.7)
        } else {
            0.0
        };
        let dark_flag = if theme.dark { 1.0 } else { 0.0 };
        self.uniform = SiteUniform {
            view_origin: vec4(vp_w_dev, vp_h_dev, 0.0, 0.0),
            pulse: vec4(pulse.cx, pulse.cy, pulse.scale, glow),
            pulse_ink: vec4(theme.ink_1.0[0], theme.ink_1.0[1], theme.ink_1.0[2], dark_flag),
            // `den` Slug tuning: [embolden_em, gamma_dark, gamma_light, embolden_max_px]
            slug: vec4(0.008, 0.88, 1.12, 0.22),
        };

        Ok(wants_frame)
    }

    #[inline]
    fn push_inst(&mut self, inst: SiteInst) -> Result<(), SiteError> {
        self.insts.try_push(inst).map_err(|r| SiteError::Alloc(r.why))
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_shape(
        &mut self,
        s: Shape,
        dx: f32,
        dy: f32,
        alpha: f32,
        scale: f32,
        theme: SiteTheme,
        hover_link: Option<u32>,
        hover_button: bool,
        p2d_x: &impl Fn(f32) -> f32,
        p2d_y: &impl Fn(f32) -> f32,
    ) -> Result<(), SiteError> {
        let mut fill_t = s.fill;
        if s.kind == ShapeKind::Underline && s.link.is_some() && s.link == hover_link {
            fill_t = Token::Ink1;
        }
        if s.kind == ShapeKind::Button && hover_button {
            fill_t = Token::Ink1;
        }
        let press_mul = if s.kind == ShapeKind::Button && self.press_button { 0.88 } else { 1.0 };
        let a = s.alpha * alpha * press_mul;
        let mut x = p2d_x(s.rect.x + dx);
        let mut y = p2d_y(s.rect.y + dy);
        let mut w = s.rect.w * scale;
        let mut h = s.rect.h * scale;
        if matches!(s.kind, ShapeKind::Rule | ShapeKind::Underline | ShapeKind::Divider) {
            x = gpu::math::round(x);
            y = gpu::math::round(y);
            if s.kind == ShapeKind::Divider {
                w = gpu::math::round(w).max(1.0);
                h = gpu::math::round(h);
            } else {
                w = gpu::math::round(w);
                h = gpu::math::round(h).max(1.0);
            }
        }
        let r = s.r * scale;
        if s.hollow {
            self.push_inst(SiteInst {
                rect: vec4(x, y, w, h),
                a: vec4(r, r, r, r),
                color: color_vec(theme.color(fill_t), a),
                fx: vec4(site_kind::RING, 0.0, scale, 0.0),
                band_tf: ZERO_V4,
                gdata: ZERO_U4,
            })?;
        } else {
            self.push_inst(SiteInst {
                rect: vec4(x, y, w, h),
                a: vec4(r, r, r, r),
                color: color_vec(theme.color(fill_t), a),
                fx: vec4(site_kind::PLATE, 0.0, 0.0, 0.0),
                band_tf: ZERO_V4,
                gdata: ZERO_U4,
            })?;
        }
        if let Some(ring_t) = s.ring {
            self.push_inst(SiteInst {
                rect: vec4(x, y, w, h),
                a: vec4(r, r, r, r),
                color: color_vec(theme.color(ring_t), alpha),
                fx: vec4(site_kind::RING, 0.0, scale, 0.0),
                band_tf: ZERO_V4,
                gdata: ZERO_U4,
            })?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_pic(
        &mut self,
        p: Pic,
        dx: f32,
        dy: f32,
        alpha: f32,
        scale: f32,
        theme: SiteTheme,
        p2d_x: &impl Fn(f32) -> f32,
        p2d_y: &impl Fn(f32) -> f32,
    ) -> Result<(), SiteError> {
        let uv = self.svg_uvs.get(p.image.index()).copied().unwrap_or([0.0; 4]);
        if uv[2] <= 0.0 || uv[3] <= 0.0 {
            return Ok(());
        }
        let is_mask = p.image.is_mask();
        let col = if is_mask {
            color_vec(theme.color(p.ink.unwrap_or(Token::Ink1)), alpha)
        } else {
            vec4(1.0, 1.0, 1.0, alpha)
        };
        self.push_inst(SiteInst {
            rect: vec4(
                gpu::math::round(p2d_x(p.rect.x + dx)),
                gpu::math::round(p2d_y(p.rect.y + dy)),
                gpu::math::round(p.rect.w * scale).max(1.0),
                gpu::math::round(p.rect.h * scale).max(1.0),
            ),
            a: vec4(uv[0], uv[1], uv[2], uv[3]),
            color: col,
            fx: vec4(site_kind::IMAGE, 0.0, if is_mask { 1.0 } else { 0.0 }, 0.0),
            band_tf: ZERO_V4,
            gdata: ZERO_U4,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_slug_glyph(
        &mut self,
        g: PlacedGlyph,
        dx: f32,
        dy: f32,
        alpha: f32,
        scale: f32,
        theme: SiteTheme,
        hover_link: Option<u32>,
        hover_button: bool,
        is_copied: bool,
        now_ms: f64,
        p2d_x: &impl Fn(f32) -> f32,
        p2d_y: &impl Fn(f32) -> f32,
    ) -> Result<bool, SiteError> {
        let mut drawn = g.glyph;
        let mut shift = 0.0f32;
        let mut is_scrambled = false;

        if g.scramble {
            let pool = self.model.scramble_pool(g.role);
            if !pool.is_empty() {
                is_scrambled = true;
                let tick = f64_to_u32_wrap(now_ms / SCRAMBLE_TICK_MS);
                let gx8 = f32_to_u32_wrap(gpu::math::round(g.x * 8.0));
                let gy8 = f32_to_u32_wrap(gpu::math::round(g.y * 8.0));
                let mut h = gx8.wrapping_mul(73_856_093)
                    ^ gy8.wrapping_mul(19_349_663)
                    ^ tick.wrapping_mul(83_492_791);
                h = (h ^ (h >> 15)).wrapping_mul(2_246_822_519);

                let own_meta = self.table.glyphs.get(usize::try_from(g.glyph).unwrap_or(usize::MAX));
                let own_w = own_meta.map_or(0.5, |m| m.bbox[2] - m.bbox[0]);
                let own_mid = own_meta.map_or(0.25, |m| (m.bbox[0] + m.bbox[2]) * 0.5);
                let tol = (0.12 * own_w).max(0.05);

                // Count similar-width glyphs in pool without allocating
                let mut like_count = 0usize;
                for &cand in pool {
                    if let Some(cm) = self.table.glyphs.get(usize::try_from(cand).unwrap_or(usize::MAX)) {
                        if ((cm.bbox[2] - cm.bbox[0]) - own_w).abs() <= tol {
                            like_count += 1;
                        }
                    }
                }
                if like_count >= 2 {
                    let target = usize::try_from(h).unwrap_or(0) % like_count;
                    let mut seen = 0usize;
                    for &cand in pool {
                        if let Some(cm) = self.table.glyphs.get(usize::try_from(cand).unwrap_or(usize::MAX)) {
                            if ((cm.bbox[2] - cm.bbox[0]) - own_w).abs() <= tol {
                                if seen == target {
                                    drawn = cand;
                                    break;
                                }
                                seen += 1;
                            }
                        }
                    }
                } else {
                    let idx = usize::try_from(h).unwrap_or(0) % pool.len();
                    if let Some(&cand) = pool.get(idx) {
                        drawn = cand;
                    }
                }
                if let Some(dm) = self.table.glyphs.get(usize::try_from(drawn).unwrap_or(usize::MAX)) {
                    let d_mid = (dm.bbox[0] + dm.bbox[2]) * 0.5;
                    shift = (own_mid - d_mid) * g.size;
                }
            }
        }

        let Some(meta) = self.table.glyphs.get(usize::try_from(drawn).unwrap_or(usize::MAX)).copied() else {
            return Ok(is_scrambled);
        };
        if !meta.drawable {
            return Ok(is_scrambled);
        }

        let ppe = (g.size * scale).max(1.0);
        let pad_em = 1.0 / ppe;
        let x0 = meta.bbox[0] - pad_em;
        let y0 = meta.bbox[1] - pad_em;
        let x1 = meta.bbox[2] + pad_em;
        let y1 = meta.bbox[3] + pad_em;
        let ox = p2d_x(g.x + shift + dx);
        let oy = gpu::math::round(p2d_y(g.y + dy));

        let spec = g.role.spec();
        let ink_tok = if g.button.is_some() {
            if hover_button { Token::Bg } else { spec.ink }
        } else if let (Some(lid), Some(ls)) = (g.link, spec.link) {
            if hover_link == Some(lid) { ls.hover } else { ls.ink }
        } else {
            spec.ink
        };
        let eff_alpha = if is_scrambled { alpha * SCRAMBLE_ALPHA } else { alpha };
        let mine = if is_copied { 1.0 } else { 0.0 };

        self.last_glyph_count = self.last_glyph_count.saturating_add(1);
        self.push_inst(SiteInst {
            rect: vec4(ox + x0 * ppe, oy - y1 * ppe, (x1 - x0) * ppe, (y1 - y0) * ppe),
            a: vec4(x0, y1, x1 - x0, y0 - y1),
            color: color_vec(theme.color(ink_tok), eff_alpha),
            fx: vec4(site_kind::SLUG_GLYPH, mine, 0.0, mine),
            band_tf: vec4(meta.band_tf[0], meta.band_tf[1], meta.band_tf[2], meta.band_tf[3]),
            gdata: uvec4(meta.gdata[0], meta.gdata[1], meta.gdata[2], meta.gdata[3]),
        })?;
        Ok(is_scrambled)
    }

    // -------------------------------------------------------------------------
    // Native WebGPU Cmd+F Find Bar & Zoom HUD Pill (Viewport-Pinned)
    // -------------------------------------------------------------------------

    fn emit_ui_slug_text(
        &mut self,
        text: &str,
        start_x_css: f32,
        baseline_y_css: f32,
        size_css: f32,
        family: u32,
        wght: u32,
        opsz: f32,
        color: Vec4,
        dpr: f32,
    ) -> Result<f32, SiteError> {
        let Some(inst) = self.table.find_instance(family, wght, opsz).copied() else {
            return Ok(start_x_css);
        };
        let ppe = (size_css * dpr).max(1.0);
        let pad_em = 1.0 / ppe;
        let mut cx = start_x_css;
        let oy = gpu::math::round(baseline_y_css * dpr);

        for ch in text.chars() {
            if ch == ' ' {
                cx += if family == 1 { 0.60 * size_css } else { 0.28 * size_css };
                continue;
            }
            if let Some(gid) = self.table.lookup_glyph(&inst, ch) {
                if let Some(meta) = self.table.glyphs.get(usize::try_from(gid).unwrap_or(usize::MAX)).copied() {
                    if meta.drawable {
                        let x0 = meta.bbox[0] - pad_em;
                        let y0 = meta.bbox[1] - pad_em;
                        let x1 = meta.bbox[2] + pad_em;
                        let y1 = meta.bbox[3] + pad_em;
                        let ox = cx * dpr;
                        self.last_glyph_count = self.last_glyph_count.saturating_add(1);
                        self.push_inst(SiteInst {
                            rect: vec4(ox + x0 * ppe, oy - y1 * ppe, (x1 - x0) * ppe, (y1 - y0) * ppe),
                            a: vec4(x0, y1, x1 - x0, y0 - y1),
                            color,
                            fx: vec4(site_kind::SLUG_GLYPH, 0.0, 0.0, 0.0),
                            band_tf: vec4(meta.band_tf[0], meta.band_tf[1], meta.band_tf[2], meta.band_tf[3]),
                            gdata: uvec4(meta.gdata[0], meta.gdata[1], meta.gdata[2], meta.gdata[3]),
                        })?;
                    }
                    let adv = if family == 1 {
                        0.60
                    } else {
                        (meta.bbox[2].max(0.32) + 0.075).clamp(0.26, 0.92)
                    };
                    cx += adv * size_css;
                    continue;
                }
            }
            cx += 0.50 * size_css;
        }
        Ok(cx)
    }

    fn emit_find_bar(&mut self, dpr: f32, theme: SiteTheme, now_ms: f64) -> Result<(), SiteError> {
        let g = self.find_bar_geom();
        let r = 10.0 * dpr;

        // Soft SDF shadow (matching intro.css `.page-find`)
        let sr = r + 14.0 * dpr;
        self.push_inst(SiteInst {
            rect: vec4(
                (g.bar.x - 14.0) * dpr,
                (g.bar.y + 8.0 - 14.0) * dpr,
                (g.bar.w + 28.0) * dpr,
                (g.bar.h + 28.0) * dpr,
            ),
            a: vec4(sr, sr, sr, sr),
            color: vec4(0.0, 0.0, 0.0, if theme.dark { 0.42 } else { 0.16 }),
            fx: vec4(site_kind::SHADOW, 0.0, 14.0 * dpr, 0.0),
            band_tf: ZERO_V4,
            gdata: ZERO_U4,
        })?;

        // Bar body (`bg-2` + crisp 1px `line-2` ring)
        self.push_inst(SiteInst {
            rect: vec4(g.bar.x * dpr, g.bar.y * dpr, g.bar.w * dpr, g.bar.h * dpr),
            a: vec4(r, r, r, r),
            color: color_vec(theme.bg_2, 0.98),
            fx: vec4(site_kind::PLATE, 0.0, 0.0, 0.0),
            band_tf: ZERO_V4,
            gdata: ZERO_U4,
        })?;
        self.push_inst(SiteInst {
            rect: vec4(g.bar.x * dpr, g.bar.y * dpr, g.bar.w * dpr, g.bar.h * dpr),
            a: vec4(r, r, r, r),
            color: color_vec(theme.line_2, 1.0),
            fx: vec4(site_kind::RING, 0.0, 1.0 * dpr, 0.0),
            band_tf: ZERO_V4,
            gdata: ZERO_U4,
        })?;

        // Magnifier icon (hollow ring + small accent dot)
        let mx = g.bar.x + 12.0;
        let my = g.bar.y + 14.0;
        let mr = 5.0 * dpr;
        self.push_inst(SiteInst {
            rect: vec4(mx * dpr, my * dpr, 10.0 * dpr, 10.0 * dpr),
            a: vec4(mr, mr, mr, mr),
            color: color_vec(theme.ink_3, 1.0),
            fx: vec4(site_kind::RING, 0.0, 1.4 * dpr, 0.0),
            band_tf: ZERO_V4,
            gdata: ZERO_U4,
        })?;
        self.push_inst(SiteInst {
            rect: vec4((mx + 8.5) * dpr, (my + 8.5) * dpr, 3.5 * dpr, 1.6 * dpr),
            a: vec4(0.8 * dpr, 0.8 * dpr, 0.8 * dpr, 0.8 * dpr),
            color: color_vec(theme.ink_3, 1.0),
            fx: vec4(site_kind::PLATE, 0.0, 0.0, 0.0),
            band_tf: ZERO_V4,
            gdata: ZERO_U4,
        })?;

        // Query string or placeholder + blinking caret in Slug vector glyphs
        let text_x = g.bar.x + 30.0;
        let baseline_y = g.bar.y + 25.0;
        let mut q_copy = OwnString::new_in(self.alloc);
        q_copy.try_push_str(self.find_query.as_str())?;
        let caret_x = if q_copy.is_empty() {
            let _ = self.emit_ui_slug_text(
                "Find",
                text_x,
                baseline_y,
                13.0,
                0,
                400,
                14.0,
                color_vec(theme.ink_4, 1.0),
                dpr,
            )?;
            text_x
        } else {
            self.emit_ui_slug_text(
                q_copy.as_str(),
                text_x,
                baseline_y,
                13.0,
                0,
                400,
                14.0,
                color_vec(theme.ink_1, 1.0),
                dpr,
            )?
        };

        // Blinking caret (530ms period)
        if (f64_to_u32_wrap(now_ms / 530.0) & 1) == 0 {
            self.push_inst(SiteInst {
                rect: vec4((caret_x + 1.0) * dpr, (g.bar.y + 11.0) * dpr, 1.5 * dpr, 18.0 * dpr),
                a: vec4(0.5 * dpr, 0.5 * dpr, 0.5 * dpr, 0.5 * dpr),
                color: color_vec(theme.fire, 1.0),
                fx: vec4(site_kind::PLATE, 0.0, 0.0, 0.0),
                band_tf: ZERO_V4,
                gdata: ZERO_U4,
            })?;
        }

        // Match count `"2 of 3"` or `"0 of 0"`
        if !q_copy.is_empty() {
            let mut cnt = gpu::emit::StackWriter::<32>::new();
            let total = self.find_matches.len();
            if total == 0 {
                let _ = write!(&mut cnt, "0/0");
            } else {
                let _ = write!(&mut cnt, "{} of {}", self.find_current + 1, total);
            }
            let cnt_x = g.prev_btn.x - 46.0;
            let _ = self.emit_ui_slug_text(
                cnt.as_str(),
                cnt_x,
                baseline_y,
                12.0,
                0,
                400,
                14.0,
                color_vec(if total == 0 { theme.fire } else { theme.ink_3 }, 1.0),
                dpr,
            )?;
        }

        // Divider before buttons
        self.push_inst(SiteInst {
            rect: vec4(gpu::math::round((g.prev_btn.x - 5.0) * dpr), (g.bar.y + 11.0) * dpr, 1.0, 18.0 * dpr),
            a: ZERO_V4,
            color: color_vec(theme.line, 1.0),
            fx: vec4(site_kind::PLATE, 0.0, 0.0, 0.0),
            band_tf: ZERO_V4,
            gdata: ZERO_U4,
        })?;

        // Prev (`<`), Next (`>`), Close (`x`) buttons
        let btns = [(1u8, g.prev_btn, "<"), (2u8, g.next_btn, ">"), (3u8, g.close_btn, "x")];
        for (id, br, lbl) in btns {
            if self.find_hover_btn == id {
                let br_r = 6.0 * dpr;
                self.push_inst(SiteInst {
                    rect: vec4(br.x * dpr, br.y * dpr, br.w * dpr, br.h * dpr),
                    a: vec4(br_r, br_r, br_r, br_r),
                    color: color_vec(theme.s_tint, 1.0),
                    fx: vec4(site_kind::PLATE, 0.0, 0.0, 0.0),
                    band_tf: ZERO_V4,
                    gdata: ZERO_U4,
                })?;
            }
            let ink = if self.find_hover_btn == id { theme.ink_1 } else { theme.ink_3 };
            let _ = self.emit_ui_slug_text(
                lbl,
                br.x + 9.0,
                br.y + 17.5,
                13.0,
                1,
                400,
                0.0,
                color_vec(ink, 1.0),
                dpr,
            )?;
        }
        Ok(())
    }

    fn emit_zoom_pill(&mut self, zr: Rect, dpr: f32, theme: SiteTheme) -> Result<(), SiteError> {
        let r = (zr.h * 0.5) * dpr;
        let sr = r + 10.0 * dpr;
        self.push_inst(SiteInst {
            rect: vec4((zr.x - 10.0) * dpr, (zr.y + 4.0 - 10.0) * dpr, (zr.w + 20.0) * dpr, (zr.h + 20.0) * dpr),
            a: vec4(sr, sr, sr, sr),
            color: vec4(0.0, 0.0, 0.0, 0.28),
            fx: vec4(site_kind::SHADOW, 0.0, 10.0 * dpr, 0.0),
            band_tf: ZERO_V4,
            gdata: ZERO_U4,
        })?;
        self.push_inst(SiteInst {
            rect: vec4(zr.x * dpr, zr.y * dpr, zr.w * dpr, zr.h * dpr),
            a: vec4(r, r, r, r),
            color: color_vec(if self.zoom_pill_hover { theme.bg_3 } else { theme.bg_2 }, 0.96),
            fx: vec4(site_kind::PLATE, 0.0, 0.0, 0.0),
            band_tf: ZERO_V4,
            gdata: ZERO_U4,
        })?;
        self.push_inst(SiteInst {
            rect: vec4(zr.x * dpr, zr.y * dpr, zr.w * dpr, zr.h * dpr),
            a: vec4(r, r, r, r),
            color: color_vec(if self.zoom_pill_hover { theme.ink_3 } else { theme.line_2 }, 1.0),
            fx: vec4(site_kind::RING, 0.0, 1.0 * dpr, 0.0),
            band_tf: ZERO_V4,
            gdata: ZERO_U4,
        })?;

        let pct = f32_to_u32_wrap(gpu::math::round(self.zoom() * 100.0));
        let mut label = gpu::emit::StackWriter::<24>::new();
        let _ = write!(&mut label, "{}%  Reset", pct);
        let _ = self.emit_ui_slug_text(
            label.as_str(),
            zr.x + 13.0,
            zr.y + 19.5,
            12.0,
            1,
            400,
            0.0,
            color_vec(theme.ink_1, 1.0),
            dpr,
        )?;
        Ok(())
    }
}

#[inline]
fn point_in_rect(x: f32, y: f32, r: Rect) -> bool {
    x >= r.x && x <= r.x + r.w && y >= r.y && y <= r.y + r.h
}

#[inline]
fn ease_out(t: f32) -> f32 {
    let u = 1.0 - t.clamp(0.0, 1.0);
    1.0 - u * u * u
}

#[inline]
fn color_vec(c: Rgba, alpha_mul: f32) -> Vec4 {
    vec4(c.0[0], c.0[1], c.0[2], c.0[3] * alpha_mul)
}

fn usize_to_f32(v: usize) -> f32 {
    let u = u32::try_from(v).unwrap_or(u32::MAX);
    let hi = u16::try_from(u >> 16).unwrap_or(0);
    let lo = u16::try_from(u & 0xffff).unwrap_or(0);
    f32::from(hi) * 65536.0 + f32::from(lo)
}

fn f32_to_u32_wrap(v: f32) -> u32 {
    if !v.is_finite() || v <= 0.0 {
        return 0;
    }
    let clamped = v.min(16_777_215.0);
    // Exact IEEE-754 f32 -> integer extraction (zero `as` cast!)
    let bits = clamped.to_bits();
    let exp = i32::try_from((bits >> 23) & 0xff).unwrap_or(0) - 127;
    if exp < 0 {
        return 0;
    }
    let mant = (bits & 0x007f_ffff) | 0x0080_0000;
    if exp >= 23 {
        let sh = u32::try_from(exp - 23).unwrap_or(0).min(8);
        mant.wrapping_shl(sh)
    } else {
        let sh = u32::try_from(23 - exp).unwrap_or(0);
        mant >> sh
    }
}

fn f64_to_f32(v: f64) -> f32 {
    if v == 0.0 {
        return if v.is_sign_negative() { -0.0 } else { 0.0 };
    }
    let bits = v.to_bits();
    let sign = u32::try_from((bits >> 63) & 1).unwrap_or(0) << 31;
    let exp64 = i32::try_from((bits >> 52) & 0x7ff).unwrap_or(0);
    let mant64 = bits & 0x000f_ffff_ffff_ffff;
    if exp64 == 0x7ff {
        let nan = if mant64 != 0 { 0x0040_0000 } else { 0 };
        return f32::from_bits(sign | 0x7f80_0000 | nan);
    }
    let exp32 = exp64 - 1023 + 127;
    if exp32 <= 0 {
        return f32::from_bits(sign);
    }
    if exp32 >= 255 {
        return f32::from_bits(sign | 0x7f80_0000);
    }
    let mant32 = u32::try_from((mant64 >> 29) & 0x007f_ffff).unwrap_or(0);
    let e = u32::try_from(exp32).unwrap_or(0);
    f32::from_bits(sign | (e << 23) | mant32)
}

fn f64_to_u32_wrap(v: f64) -> u32 {
    if !v.is_finite() || v <= 0.0 {
        return 0;
    }
    let bits = v.to_bits();
    let exp = i32::try_from((bits >> 52) & 0x7ff).unwrap_or(0) - 1023;
    if exp < 0 {
        return 0;
    }
    let mant = (bits & 0x000f_ffff_ffff_ffff) | 0x0010_0000_0000_0000;
    let shifted = if exp >= 52 {
        let sh = u32::try_from(exp - 52).unwrap_or(0).min(31);
        mant.wrapping_shl(sh)
    } else {
        let sh = u32::try_from(52 - exp).unwrap_or(0);
        mant >> sh
    };
    u32::try_from(shifted & 0xffff_ffff).unwrap_or(0)
}
