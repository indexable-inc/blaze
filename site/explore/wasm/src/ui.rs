//! 100% Pure-Rust WebGPU Modular Docking Observatory UI & Input Engine.
//!
//! Powered by `dock::DockTree`: EVERY pane (`Timeline`, `FlameGraph`, `CodeMap`, `CpuTopology`,
//! `HotSpots`, `HeapDiff`, `Builds`, `Advisor`, `IoLocks`, `Inspector`) is modular, resizable
//! in both X & Y, expandable/maximizable to full viewport, splittable side-by-side, closeable,
//! and drag-and-drop dockable to any Left / Right / Top / Bottom / Center drop zone.

use std::collections::HashMap;

use crate::dock::{
    compute_drop_zone, drop_preview_rect, DockTree, DropZone, LeafLayout, PaneId, Rect, SplitDir,
    SplitterLayout,
};
use crate::ecs::Rel;
use crate::gpu::{col, DrawList, Icon};
use crate::model::{
    fmt_bytes, fmt_ns, fmt_span, io_op_name, percentile, short_path, BarKind, FlameNode, FnComp,
    Insight, ProfilerWorld, TimelineBar,
};
use crate::telemetry::CPU_UNKNOWN;

const TOPBAR_H: f32 = 38.0;
const MINIMAP_H: f32 = 22.0;
const RULER_H: f32 = 22.0;
const GUTTER_W: f32 = 96.0;

const MIN_SPAN_MS: f64 = 0.2;
const MAX_SPAN_MS: f64 = 60_000.0;
const DEFAULT_SPAN_MS: f64 = 3_500.0;
const PLAYOUT_BUFFER_MS: f64 = 120.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DragMode {
    Pan,
    Select,
}

#[derive(Clone, Copy, PartialEq)]
enum DragOp {
    None,
    Splitter {
        split_id: u32,
        dir: SplitDir,
        parent: Rect,
    },
    TabPending {
        from_leaf: u32,
        pane: PaneId,
    },
    TabDock {
        from_leaf: u32,
        pane: PaneId,
    },
    TimelinePan {
        plot_w: f32,
    },
    TimelineSelect {
        plot_x: f32,
        plot_w: f32,
    },
    CodeMapPan,
    VScroll {
        pane: PaneId,
        track_y: f32,
        track_h: f32,
        thumb_h: f32,
        max_scroll: f32,
        grab_off: f32,
    },
    HScrollSource {
        track_x: f32,
        track_w: f32,
        thumb_w: f32,
        max_scroll: f32,
        grab_off: f32,
    },
    MinimapScrub {
        bar_x: f32,
        bar_w: f32,
        t_min: f64,
        t_max: f64,
        /// 0 = drag/jump center, 1 = resize left edge, 2 = resize right edge
        edge: u8,
        anchor_start: f64,
        anchor_end: f64,
        grab_t: f64,
    },
    MapRadar {
        box_x: f32,
        box_y: f32,
        box_w: f32,
        box_h: f32,
        world_x: f32,
        world_y: f32,
        world_w: f32,
        world_h: f32,
        view_x: f32,
        view_y: f32,
        view_w: f32,
        view_h: f32,
    },
}

#[derive(Clone, Copy)]
struct VScrollHit {
    pane: PaneId,
    track: Rect,
    thumb: Rect,
    max_scroll: f32,
}

#[derive(Clone, Copy)]
struct HScrollHit {
    track: Rect,
    thumb: Rect,
    max_scroll: f32,
}

#[derive(Clone, Copy)]
struct MinimapHit {
    bar: Rect,
    win_x0: f32,
    win_x1: f32,
    t_min: f64,
    t_max: f64,
}

#[derive(Clone, Copy)]
struct RadarHit {
    rect: Rect,
    world_x: f32,
    world_y: f32,
    world_w: f32,
    world_h: f32,
    view_x: f32,
    view_y: f32,
    view_w: f32,
    view_h: f32,
}

#[derive(Clone, Debug)]
enum Action {
    SetMode(DragMode),
    ZoomIn,
    ZoomOut,
    TogglePause,
    ClearTrace,
    ToggleLayoutMenu,
    ResetLayout,
    OpenPane(PaneId),
    FollowLive,
    SelectTab(u32, usize, PaneId),
    PopOutPane(u32, PaneId),
    MaximizePane(PaneId),
    ClosePane(PaneId),
    ZoomSelection,
    ClearSelection,
    FlameMode(bool),
    FlameZoom(Option<String>),
    InspectSlot(u32),
    SetHook(u32, u8),
    OpenEditor(String, u32),
    HeapTakeSnap,
    HeapClearSnap,
    JumpToTraceTime(f64),
}

struct HitZone {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    action: Action,
    cursor: &'static str,
}

#[derive(Clone)]
enum HoverTarget {
    Bar(TimelineBar),
    ThreadGutter(u32),
    Flame(String, u64, u64, u64, u64),
}

pub struct UiApp {
    pub world: ProfilerWorld,
    pub draw: DrawList,
    pub dock: DockTree,
    pub webgpu_active: bool,

    // Timeline camera
    pub view_span_ms: f64,
    pub view_end_ms: f64,
    pub following: bool,
    pub paused: bool,
    pub mode: DragMode,
    pub selection_ms: Option<(f64, f64)>,

    // Per-pane independent scroll positions & limits
    pane_scroll: HashMap<PaneId, f32>,
    pane_max_scroll: HashMap<PaneId, f32>,
    source_scroll_x: f32,
    source_max_scroll_x: f32,

    // Active inspection & popover state
    pub inspected_slot: Option<u32>,
    pub layout_menu_open: bool,

    // Flame Graph state
    pub flame_inverted: bool,
    pub flame_zoom_name: Option<String>,

    // Code Map camera
    pub map_pan_x: f32,
    pub map_pan_y: f32,
    pub map_scale: f32,

    // Pointer & drag state
    pub mouse_x: f32,
    pub mouse_y: f32,
    pub mouse_inside: bool,
    drag_op: DragOp,
    drag_start_x: f32,
    drag_start_y: f32,
    drag_start_end_ms: f64,
    drag_start_scroll_y: f32,
    drag_start_map_x: f32,
    drag_start_map_y: f32,
    sel_anchor_ms: f64,
    last_now_ms: f64,
    last_css_w: f32,
    last_css_h: f32,

    // Last computed dock & scrollbar/minimap geometry for instant hit-testing
    leaves: Vec<LeafLayout>,
    splitters: Vec<SplitterLayout>,
    vscrolls: Vec<VScrollHit>,
    hscroll_source: Option<HScrollHit>,
    minimap_hit: Option<MinimapHit>,
    radar_hit: Option<RadarHit>,

    hits: Vec<HitZone>,
    hover_target: Option<HoverTarget>,
    hovered_bar: Option<TimelineBar>,
    pub desired_cursor: &'static str,

    pub commands: Vec<String>,
}

impl Default for UiApp {
    fn default() -> Self {
        Self::new()
    }
}

impl UiApp {
    pub fn new() -> Self {
        Self {
            world: ProfilerWorld::new(),
            draw: DrawList::new(),
            dock: DockTree::default_layout(),
            webgpu_active: true,
            view_span_ms: DEFAULT_SPAN_MS,
            view_end_ms: 0.0,
            following: true,
            paused: false,
            mode: DragMode::Pan,
            selection_ms: None,
            pane_scroll: HashMap::new(),
            pane_max_scroll: HashMap::new(),
            source_scroll_x: 0.0,
            source_max_scroll_x: 0.0,
            inspected_slot: None,
            layout_menu_open: false,
            flame_inverted: false,
            flame_zoom_name: None,
            map_pan_x: 0.0,
            map_pan_y: 0.0,
            map_scale: 1.0,
            mouse_x: -1.0,
            mouse_y: -1.0,
            mouse_inside: false,
            drag_op: DragOp::None,
            drag_start_x: 0.0,
            drag_start_y: 0.0,
            drag_start_end_ms: 0.0,
            drag_start_scroll_y: 0.0,
            drag_start_map_x: 0.0,
            drag_start_map_y: 0.0,
            sel_anchor_ms: 0.0,
            last_now_ms: 0.0,
            last_css_w: 1280.0,
            last_css_h: 800.0,
            leaves: Vec::new(),
            splitters: Vec::new(),
            vscrolls: Vec::new(),
            hscroll_source: None,
            minimap_hit: None,
            radar_hit: None,
            hits: Vec::with_capacity(256),
            hover_target: None,
            hovered_bar: None,
            desired_cursor: "default",
            commands: Vec::new(),
        }
    }

    fn playhead_edge(&self, now_ms: f64) -> f64 {
        now_ms.max(self.world.latest_ts_ms) - PLAYOUT_BUFFER_MS
    }

    fn scroll_of(&self, pane: PaneId) -> f32 {
        self.pane_scroll.get(&pane).copied().unwrap_or(0.0)
    }

    fn set_scroll(&mut self, pane: PaneId, y: f32) {
        let max_y = self.pane_max_scroll.get(&pane).copied().unwrap_or(0.0);
        self.pane_scroll.insert(pane, y.clamp(0.0, max_y));
    }

    fn queue_source_if_needed(&mut self, file: &str, line: u32) {
        if file.is_empty() || line == 0 {
            return;
        }
        let key = (file.to_string(), line);
        if self.world.sources.contains_key(&key) || self.world.pending_sources.contains(&key) {
            return;
        }
        self.world.pending_sources.insert(key);
        self.commands.push(format!("source:{file}|{line}"));
    }

    fn hit_point(&self, x: f32, y: f32) -> Option<&HitZone> {
        self.hits
            .iter()
            .rev()
            .find(|h| x >= h.x && x <= h.x + h.w && y >= h.y && y <= h.y + h.h)
    }

    fn push_hit(&mut self, x: f32, y: f32, w: f32, h: f32, action: Action, cursor: &'static str) {
        self.hits.push(HitZone {
            x,
            y,
            w,
            h,
            action,
            cursor,
        });
    }

    fn is_hovered(&self, x: f32, y: f32, w: f32, h: f32) -> bool {
        self.mouse_inside
            && self.mouse_x >= x
            && self.mouse_x <= x + w
            && self.mouse_y >= y
            && self.mouse_y <= y + h
    }

    fn leaf_at(&self, x: f32, y: f32) -> Option<&LeafLayout> {
        self.leaves.iter().find(|lf| lf.rect.contains(x, y))
    }

    fn splitter_at(&self, x: f32, y: f32) -> Option<SplitterLayout> {
        self.splitters
            .iter()
            .copied()
            .find(|s| x >= s.rect.x - 3.0 && x <= s.rect.x + s.rect.w + 3.0 && y >= s.rect.y - 3.0 && y <= s.rect.y + s.rect.h + 3.0)
    }

    // -----------------------------------------------------------------------
    // Input Routing (Dynamic Across Any Docked / Split / Maximized Layout!)
    // -----------------------------------------------------------------------

    pub fn on_pointer_move(&mut self, x: f32, y: f32) {
        self.mouse_x = x;
        self.mouse_y = y;
        self.mouse_inside = x >= 0.0 && y >= 0.0;

        match self.drag_op {
            DragOp::Splitter { split_id, dir, parent } => {
                let ratio = match dir {
                    SplitDir::Horizontal => (x - parent.x) / parent.w.max(1.0),
                    SplitDir::Vertical => (y - parent.y) / parent.h.max(1.0),
                };
                self.dock.set_ratio(split_id, ratio);
                self.desired_cursor = if dir == SplitDir::Horizontal {
                    "col-resize"
                } else {
                    "row-resize"
                };
            }
            DragOp::TabPending { from_leaf, pane } => {
                let dist = ((x - self.drag_start_x).powi(2) + (y - self.drag_start_y).powi(2)).sqrt();
                if dist >= 6.0 {
                    self.drag_op = DragOp::TabDock { from_leaf, pane };
                    self.desired_cursor = "grabbing";
                }
            }
            DragOp::TabDock { .. } => {
                self.desired_cursor = "grabbing";
            }
            DragOp::TimelinePan { plot_w } => {
                let dx = (x - self.drag_start_x) as f64;
                let dy = y - self.drag_start_y;
                let live = self.playhead_edge(self.last_now_ms);
                self.view_end_ms = (self.drag_start_end_ms - (dx / plot_w.max(1.0) as f64) * self.view_span_ms)
                    .min(live + PLAYOUT_BUFFER_MS);
                self.following = live - self.view_end_ms < self.view_span_ms * 0.015;
                self.set_scroll(PaneId::Timeline, self.drag_start_scroll_y - dy);
                self.desired_cursor = "grabbing";
            }
            DragOp::TimelineSelect { plot_x, plot_w } => {
                let view_start = self.view_end_ms - self.view_span_ms;
                let norm = (((x - plot_x) / plot_w.max(1.0)) as f64).clamp(0.0, 1.0);
                let cur_ts = view_start + norm * self.view_span_ms;
                self.selection_ms = Some(if cur_ts >= self.sel_anchor_ms {
                    (self.sel_anchor_ms, cur_ts)
                } else {
                    (cur_ts, self.sel_anchor_ms)
                });
                self.desired_cursor = "col-resize";
            }
            DragOp::CodeMapPan => {
                self.map_pan_x = self.drag_start_map_x + (x - self.drag_start_x);
                self.map_pan_y = self.drag_start_map_y + (y - self.drag_start_y);
                self.desired_cursor = "grabbing";
            }
            DragOp::VScroll {
                pane,
                track_y,
                track_h,
                thumb_h,
                max_scroll,
                grab_off,
            } => {
                let travel = (track_h - thumb_h).max(1.0);
                let frac = ((y - grab_off - track_y) / travel).clamp(0.0, 1.0);
                self.set_scroll(pane, frac * max_scroll);
                self.desired_cursor = "grabbing";
            }
            DragOp::HScrollSource {
                track_x,
                track_w,
                thumb_w,
                max_scroll,
                grab_off,
            } => {
                let travel = (track_w - thumb_w).max(1.0);
                let frac = ((x - grab_off - track_x) / travel).clamp(0.0, 1.0);
                self.source_scroll_x = (frac * max_scroll).clamp(0.0, max_scroll);
                self.desired_cursor = "grabbing";
            }
            DragOp::MinimapScrub {
                bar_x,
                bar_w,
                t_min,
                t_max,
                edge,
                anchor_start,
                anchor_end,
                grab_t,
            } => {
                let total = (t_max - t_min).max(1.0);
                let norm = (((x - bar_x) / bar_w.max(1.0)) as f64).clamp(0.0, 1.0);
                let cur_t = t_min + norm * total;
                let live = self.playhead_edge(self.last_now_ms);
                if edge == 1 {
                    // Drag left handle -> adjust view_start while keeping view_end fixed
                    let new_start = cur_t.min(anchor_end - MIN_SPAN_MS).max(t_min);
                    self.view_span_ms = (anchor_end - new_start).clamp(MIN_SPAN_MS, MAX_SPAN_MS);
                    self.view_end_ms = anchor_end;
                    self.following = false;
                    self.desired_cursor = "ew-resize";
                } else if edge == 2 {
                    // Drag right handle -> adjust view_end while keeping view_start fixed
                    let new_end = cur_t.max(anchor_start + MIN_SPAN_MS).min(live + PLAYOUT_BUFFER_MS);
                    self.view_span_ms = (new_end - anchor_start).clamp(MIN_SPAN_MS, MAX_SPAN_MS);
                    self.view_end_ms = new_end;
                    self.following = live - self.view_end_ms < self.view_span_ms * 0.015;
                    self.desired_cursor = "ew-resize";
                } else {
                    // Drag/scrub viewport center
                    let dt = cur_t - grab_t;
                    let span = (anchor_end - anchor_start).clamp(MIN_SPAN_MS, MAX_SPAN_MS);
                    let mut next_end = (anchor_end + dt).clamp(t_min + span, live + PLAYOUT_BUFFER_MS);
                    if live - next_end < span * 0.015 {
                        next_end = live;
                        self.following = true;
                    } else {
                        self.following = false;
                    }
                    self.view_end_ms = next_end;
                    self.desired_cursor = "grabbing";
                }
            }
            DragOp::MapRadar {
                box_x,
                box_y,
                box_w,
                box_h,
                world_x,
                world_y,
                world_w,
                world_h,
                view_x,
                view_y,
                view_w,
                view_h,
            } => {
                let nx = ((x - box_x) / box_w.max(1.0)).clamp(0.0, 1.0);
                let ny = ((y - box_y) / box_h.max(1.0)).clamp(0.0, 1.0);
                let target_wx = world_x + nx * world_w;
                let target_wy = world_y + ny * world_h;
                let center_vx = view_x + view_w * 0.5;
                let center_vy = view_y + view_h * 0.5;
                self.map_pan_x += center_vx - target_wx;
                self.map_pan_y += center_vy - target_wy;
                self.desired_cursor = "grabbing";
            }
            DragOp::None => {
                if let Some(h) = self.hit_point(x, y) {
                    self.desired_cursor = h.cursor;
                } else if let Some(mm) = self.minimap_hit {
                    if mm.bar.contains(x, y) {
                        if (x - mm.win_x0).abs() <= 5.0 || (x - mm.win_x1).abs() <= 5.0 {
                            self.desired_cursor = "ew-resize";
                        } else {
                            self.desired_cursor = "grab";
                        }
                        return;
                    }
                    self.update_idle_cursor(x, y);
                } else {
                    self.update_idle_cursor(x, y);
                }
            }
        }
    }

    fn update_idle_cursor(&mut self, x: f32, y: f32) {
        if self.vscrolls.iter().any(|v| v.track.contains(x, y))
            || self.hscroll_source.map(|h| h.track.contains(x, y)).unwrap_or(false)
            || self.radar_hit.map(|r| r.rect.contains(x, y)).unwrap_or(false)
        {
            self.desired_cursor = "pointer";
        } else if let Some(sp) = self.splitter_at(x, y) {
            self.desired_cursor = if sp.dir == SplitDir::Horizontal {
                "col-resize"
            } else {
                "row-resize"
            };
        } else if let Some(lf) = self.leaf_at(x, y) {
            if lf.body.contains(x, y) {
                match lf.active_pane() {
                    Some(PaneId::Timeline) if x >= lf.body.x + GUTTER_W && y >= lf.body.y + MINIMAP_H + RULER_H => {
                        self.desired_cursor = if self.mode == DragMode::Select {
                            "crosshair"
                        } else {
                            "grab"
                        };
                    }
                    Some(PaneId::CodeMap) => self.desired_cursor = "grab",
                    _ => self.desired_cursor = "default",
                }
            } else {
                self.desired_cursor = "default";
            }
        } else {
            self.desired_cursor = "default";
        }
    }

    pub fn on_pointer_down(&mut self, x: f32, y: f32, shift: bool) {
        self.mouse_x = x;
        self.mouse_y = y;
        self.mouse_inside = true;
        self.drag_start_x = x;
        self.drag_start_y = y;

        // 1. Check explicit clickable controls (buttons, tabs, popover items, cards, rows)
        if let Some(action) = self.hit_point(x, y).map(|h| h.action.clone()) {
            match action {
                Action::SelectTab(leaf_id, idx, pane) => {
                    self.layout_menu_open = false;
                    self.dock.select_tab(leaf_id, idx);
                    // Arm drag-to-dock if the user drags this tab > 6px!
                    self.drag_op = DragOp::TabPending {
                        from_leaf: leaf_id,
                        pane,
                    };
                }
                other => {
                    if !matches!(other, Action::ToggleLayoutMenu) {
                        self.layout_menu_open = false;
                    }
                    self.execute_action(other);
                }
            }
            return;
        }
        self.layout_menu_open = false;

        // 2. Interactive Scrollbars, Timeline Overview Minimap, and Code Map Radar
        if let Some(mm) = self.minimap_hit {
            if mm.bar.contains(x, y) {
                let total = (mm.t_max - mm.t_min).max(1.0);
                let norm = (((x - mm.bar.x) / mm.bar.w.max(1.0)) as f64).clamp(0.0, 1.0);
                let click_t = mm.t_min + norm * total;
                let live = self.playhead_edge(self.last_now_ms);
                let edge = if (x - mm.win_x0).abs() <= 5.0 {
                    1
                } else if (x - mm.win_x1).abs() <= 5.0 {
                    2
                } else {
                    0
                };
                if edge == 0 && (x < mm.win_x0 || x > mm.win_x1) {
                    // Click outside viewport bracket -> jump center straight there!
                    let next_end = (click_t + self.view_span_ms * 0.5).clamp(mm.t_min + self.view_span_ms, live + PLAYOUT_BUFFER_MS);
                    self.view_end_ms = next_end;
                    self.following = live - next_end < self.view_span_ms * 0.015;
                }
                let anchor_end = if self.following { live } else { self.view_end_ms };
                let anchor_start = anchor_end - self.view_span_ms;
                self.drag_op = DragOp::MinimapScrub {
                    bar_x: mm.bar.x,
                    bar_w: mm.bar.w,
                    t_min: mm.t_min,
                    t_max: mm.t_max,
                    edge,
                    anchor_start,
                    anchor_end,
                    grab_t: click_t,
                };
                return;
            }
        }
        if let Some(vs) = self.vscrolls.iter().copied().find(|v| v.track.contains(x, y)) {
            let grab_off = if vs.thumb.contains(x, y) {
                y - vs.thumb.y
            } else {
                // Click on track -> jump thumb centered on click
                let travel = (vs.track.h - vs.thumb.h).max(1.0);
                let frac = ((y - vs.thumb.h * 0.5 - vs.track.y) / travel).clamp(0.0, 1.0);
                self.set_scroll(vs.pane, frac * vs.max_scroll);
                vs.thumb.h * 0.5
            };
            self.drag_op = DragOp::VScroll {
                pane: vs.pane,
                track_y: vs.track.y,
                track_h: vs.track.h,
                thumb_h: vs.thumb.h,
                max_scroll: vs.max_scroll,
                grab_off,
            };
            return;
        }
        if let Some(hs) = self.hscroll_source {
            if hs.track.contains(x, y) {
                let grab_off = if hs.thumb.contains(x, y) {
                    x - hs.thumb.x
                } else {
                    let travel = (hs.track.w - hs.thumb.w).max(1.0);
                    let frac = ((x - hs.thumb.w * 0.5 - hs.track.x) / travel).clamp(0.0, 1.0);
                    self.source_scroll_x = (frac * hs.max_scroll).clamp(0.0, hs.max_scroll);
                    hs.thumb.w * 0.5
                };
                self.drag_op = DragOp::HScrollSource {
                    track_x: hs.track.x,
                    track_w: hs.track.w,
                    thumb_w: hs.thumb.w,
                    max_scroll: hs.max_scroll,
                    grab_off,
                };
                return;
            }
        }
        if let Some(rd) = self.radar_hit {
            if rd.rect.contains(x, y) {
                self.drag_op = DragOp::MapRadar {
                    box_x: rd.rect.x,
                    box_y: rd.rect.y,
                    box_w: rd.rect.w,
                    box_h: rd.rect.h,
                    world_x: rd.world_x,
                    world_y: rd.world_y,
                    world_w: rd.world_w,
                    world_h: rd.world_h,
                    view_x: rd.view_x,
                    view_y: rd.view_y,
                    view_w: rd.view_w,
                    view_h: rd.view_h,
                };
                self.on_pointer_move(x, y);
                return;
            }
        }

        // 3. Any horizontal or vertical split divider
        if let Some(sp) = self.splitter_at(x, y) {
            self.drag_op = DragOp::Splitter {
                split_id: sp.split_id,
                dir: sp.dir,
                parent: sp.parent,
            };
            return;
        }

        // 4. Pane body drags (Timeline Pan/Select or Code Map Pan)
        if let Some(lf) = self.leaf_at(x, y).cloned() {
            if lf.body.contains(x, y) {
                match lf.active_pane() {
                    Some(PaneId::Timeline) => {
                        let plot_x = lf.body.x + GUTTER_W;
                        let plot_w = (lf.body.w - GUTTER_W - 10.0).max(1.0);
                        if x >= plot_x && y >= lf.body.y + MINIMAP_H + RULER_H {
                            let live = self.playhead_edge(self.last_now_ms);
                            let cur_end = if self.following { live } else { self.view_end_ms };
                            if shift || self.mode == DragMode::Select {
                                let view_start = cur_end - self.view_span_ms;
                                let norm = (((x - plot_x) / plot_w) as f64).clamp(0.0, 1.0);
                                self.sel_anchor_ms = view_start + norm * self.view_span_ms;
                                self.selection_ms = Some((self.sel_anchor_ms, self.sel_anchor_ms));
                                self.following = false;
                                self.view_end_ms = cur_end;
                                self.drag_op = DragOp::TimelineSelect { plot_x, plot_w };
                            } else {
                                self.drag_start_end_ms = cur_end;
                                self.drag_start_scroll_y = self.scroll_of(PaneId::Timeline);
                                self.drag_op = DragOp::TimelinePan { plot_w };
                            }
                        }
                    }
                    Some(PaneId::CodeMap) => {
                        self.drag_start_map_x = self.map_pan_x;
                        self.drag_start_map_y = self.map_pan_y;
                        self.drag_op = DragOp::CodeMapPan;
                    }
                    _ => {}
                }
            }
        }
    }

    pub fn on_pointer_up(&mut self, x: f32, y: f32) {
        let moved = ((x - self.drag_start_x).powi(2) + (y - self.drag_start_y).powi(2)).sqrt();
        match self.drag_op {
            DragOp::TabDock { from_leaf, pane } => {
                if let Some(target) = self.leaf_at(x, y).cloned() {
                    let zone = compute_drop_zone(target.rect, x, y);
                    self.dock.dock_pane(from_leaf, pane, target.id, zone);
                }
            }
            DragOp::TimelineSelect { .. } => {
                if let Some((s0, s1)) = self.selection_ms {
                    if s1 - s0 < self.view_span_ms * 0.005 {
                        self.selection_ms = None;
                        if moved < 5.0 {
                            self.click_hovered_bar();
                        }
                    }
                }
            }
            DragOp::TimelinePan { .. } => {
                if moved < 5.0 {
                    self.click_hovered_bar();
                }
            }
            _ => {}
        }
        self.drag_op = DragOp::None;
        self.on_pointer_move(x, y);
    }

    fn click_hovered_bar(&mut self) {
        if let Some(bar) = self.hovered_bar {
            match bar.kind {
                BarKind::Span => {
                    self.inspected_slot = Some(bar.slot_or_op);
                    self.dock.focus_or_open(PaneId::Inspector);
                }
                BarKind::Heap => self.dock.focus_or_open(PaneId::HeapDiff),
                BarKind::Lock | BarKind::Io => self.dock.focus_or_open(PaneId::IoLocks),
            }
        }
    }

    pub fn on_dblclick(&mut self, x: f32, y: f32) {
        if let Some(lf) = self.leaf_at(x, y).cloned() {
            // Double-click ANY pane header -> Expand / Restore full viewport!
            if lf.header.contains(x, y) {
                if let Some(pane) = lf.active_pane() {
                    self.dock.toggle_maximize(pane);
                }
                return;
            }
            // Double-click inside Timeline -> reset zoom & follow live
            if lf.active_pane() == Some(PaneId::Timeline) && lf.body.contains(x, y) {
                self.view_span_ms = DEFAULT_SPAN_MS;
                self.selection_ms = None;
                self.following = true;
                self.view_end_ms = self.playhead_edge(self.last_now_ms);
            } else if lf.active_pane() == Some(PaneId::CodeMap) && lf.body.contains(x, y) {
                self.map_pan_x = 0.0;
                self.map_pan_y = 0.0;
                self.map_scale = 1.0;
            }
        }
    }

    pub fn on_wheel(&mut self, x: f32, y: f32, dx: f32, dy: f32, ctrl_or_meta: bool) {
        let Some(lf) = self.leaf_at(x, y).cloned() else {
            return;
        };
        let Some(pane) = lf.active_pane() else {
            return;
        };
        match pane {
            PaneId::Timeline => {
                let plot_x = lf.body.x + GUTTER_W;
                let plot_w = (lf.body.w - GUTTER_W).max(1.0) as f64;
                let norm_x = (((x - plot_x) as f64) / plot_w).clamp(0.0, 1.0);
                // Pinch-only zoom (`ctrlKey` / `metaKey`); plain scroll pans X & scrolls Y!
                if ctrl_or_meta {
                    self.zoom_by((dy as f64 * 0.012).exp(), norm_x);
                    return;
                }
                if dx.abs() > 0.25 {
                    let shift_ms = (dx as f64 / plot_w) * self.view_span_ms;
                    let live = self.playhead_edge(self.last_now_ms);
                    let cur_end = if self.following { live } else { self.view_end_ms };
                    self.view_end_ms = (cur_end + shift_ms).min(live + PLAYOUT_BUFFER_MS);
                    self.following = live - self.view_end_ms < self.view_span_ms * 0.015;
                }
                if dy.abs() > 0.25 {
                    let cur = self.scroll_of(PaneId::Timeline);
                    self.set_scroll(PaneId::Timeline, cur + dy);
                }
            }
            PaneId::CodeMap => {
                if ctrl_or_meta {
                    self.map_scale = (self.map_scale * (-dy * 0.01).exp()).clamp(0.45, 2.4);
                } else {
                    self.map_pan_x -= dx;
                    self.map_pan_y -= dy;
                }
            }
            PaneId::Inspector => {
                if dy.abs() > 0.25 {
                    let cur = self.scroll_of(PaneId::Inspector);
                    self.set_scroll(PaneId::Inspector, cur + dy);
                }
                if dx.abs() > 0.25 && self.source_max_scroll_x > 0.0 {
                    self.source_scroll_x = (self.source_scroll_x + dx).clamp(0.0, self.source_max_scroll_x);
                }
            }
            other => {
                if dy.abs() > 0.25 {
                    let cur = self.scroll_of(other);
                    self.set_scroll(other, cur + dy);
                }
            }
        }
    }

    pub fn on_key_down(&mut self, code: u32) {
        match code as u8 {
            b'p' | b'P' => self.mode = DragMode::Pan,
            b's' | b'S' => self.mode = DragMode::Select,
            b' ' => self.paused = !self.paused,
            b'f' | b'F' => {
                self.following = true;
                self.view_end_ms = self.playhead_edge(self.last_now_ms);
            }
            27 => {
                if self.layout_menu_open {
                    self.layout_menu_open = false;
                } else if self.dock.maximized.is_some() {
                    self.dock.maximized = None;
                } else if self.selection_ms.is_some() {
                    self.selection_ms = None;
                } else if self.dock.contains_pane(PaneId::Inspector) {
                    self.dock.close_pane(PaneId::Inspector);
                }
            }
            b'+' | b'=' => self.zoom_by(0.6, 0.5),
            b'-' | b'_' => self.zoom_by(1.65, 0.5),
            _ => {}
        }
    }

    fn zoom_by(&mut self, factor: f64, anchor_norm: f64) {
        let live = self.playhead_edge(self.last_now_ms);
        let cur_end = if self.following { live } else { self.view_end_ms };
        let prev = self.view_span_ms;
        let next = (prev * factor).clamp(MIN_SPAN_MS, MAX_SPAN_MS);
        let anchor_ts = cur_end - (1.0 - anchor_norm) * prev;
        self.view_span_ms = next;
        self.view_end_ms = (anchor_ts + (1.0 - anchor_norm) * next).min(live + PLAYOUT_BUFFER_MS);
        self.following = live - self.view_end_ms < next * 0.015;
    }

    fn execute_action(&mut self, act: Action) {
        match act {
            Action::SetMode(m) => self.mode = m,
            Action::ZoomIn => self.zoom_by(0.55, 0.5),
            Action::ZoomOut => self.zoom_by(1.8, 0.5),
            Action::TogglePause => self.paused = !self.paused,
            Action::ClearTrace => {
                self.world.clear_trace();
                self.selection_ms = None;
                self.inspected_slot = None;
                self.flame_zoom_name = None;
            }
            Action::ToggleLayoutMenu => self.layout_menu_open = !self.layout_menu_open,
            Action::ResetLayout => {
                self.dock = DockTree::default_layout();
                self.layout_menu_open = false;
            }
            Action::OpenPane(pane) => {
                self.dock.focus_or_open(pane);
                self.layout_menu_open = false;
            }
            Action::FollowLive => {
                self.following = true;
                self.view_end_ms = self.playhead_edge(self.last_now_ms);
            }
            Action::SelectTab(leaf_id, idx, _) => self.dock.select_tab(leaf_id, idx),
            Action::PopOutPane(leaf_id, pane) => self.dock.pop_out_pane(leaf_id, pane),
            Action::MaximizePane(pane) => self.dock.toggle_maximize(pane),
            Action::ClosePane(pane) => {
                if pane == PaneId::Inspector {
                    self.inspected_slot = None;
                }
                self.dock.close_pane(pane);
            }
            Action::ZoomSelection => {
                if let Some((s0, s1)) = self.selection_ms {
                    let span = (s1 - s0).max(MIN_SPAN_MS);
                    let pad = span * 0.06;
                    self.view_span_ms = (span + pad * 2.0).clamp(MIN_SPAN_MS, MAX_SPAN_MS);
                    self.view_end_ms = s1 + pad;
                    self.following = false;
                }
            }
            Action::ClearSelection => self.selection_ms = None,
            Action::FlameMode(inv) => {
                self.flame_inverted = inv;
                self.flame_zoom_name = None;
            }
            Action::FlameZoom(name) => self.flame_zoom_name = name,
            Action::InspectSlot(slot) => {
                self.inspected_slot = Some(slot);
                self.dock.focus_or_open(PaneId::Inspector);
            }
            Action::SetHook(slot, hook) => {
                if let Some(&e) = self.world.slot_entity.get(&slot) {
                    if let Some(fc) = self.world.fns.get_mut(e) {
                        fc.hook = hook;
                    }
                }
                let kind_name = match hook {
                    0 => "plain",
                    1 => "count",
                    2 => "hook",
                    _ => "span",
                };
                self.commands
                    .push(format!("toggle:{}|{}|{}", self.world.run_id, slot, kind_name));
            }
            Action::OpenEditor(file, line) => {
                self.commands.push(format!("open:vscode://file/{file}:{line}"));
            }
            Action::HeapTakeSnap => {
                let secs = ((self.last_now_ms / 1000.0) as u64) % 86400;
                let lbl = format!("{:02}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60);
                self.world.take_heap_snapshot(lbl);
            }
            Action::HeapClearSnap => self.world.clear_heap_snapshot(),
            Action::JumpToTraceTime(ts_ms) => {
                self.view_span_ms = 1_500.0;
                self.view_end_ms = ts_ms + 750.0;
                self.following = false;
                self.dock.focus_or_open(PaneId::Timeline);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Full-Viewport 100% WebGPU Frame Render (Modular DockTree + Z-Pass 2 Overlays)
    // -----------------------------------------------------------------------

    pub fn render_frame(&mut self, now_ms: f64, css_w: f32, css_h: f32) {
        self.last_now_ms = now_ms;
        self.last_css_w = css_w;
        self.last_css_h = css_h;

        if self.view_end_ms == 0.0 {
            self.view_end_ms = self.playhead_edge(now_ms);
        }
        if self.following && !self.paused {
            self.view_end_ms = self.playhead_edge(now_ms);
        }

        let work_area = Rect {
            x: 0.0,
            y: TOPBAR_H,
            w: css_w,
            h: (css_h - TOPBAR_H).max(120.0),
        };
        let (leaves, splitters) = self.dock.layout(work_area);
        self.leaves = leaves.clone();
        self.splitters = splitters.clone();

        self.hits.clear();
        self.vscrolls.clear();
        self.hscroll_source = None;
        self.minimap_hit = None;
        self.radar_hit = None;
        self.hover_target = None;
        self.hovered_bar = None;
        self.draw.begin(css_w, css_h);

        // Obsidian workspace canvas
        self.draw.rect(0.0, 0.0, css_w, css_h, col::BG);

        // 1. Compact 38px Topbar
        self.render_topbar(css_w);

        // 2. Compute insights once for Advisor badge & panel
        let insights = self.world.analyze_insights();

        // 3. Render every docked Leaf Pane (modular header + active content!)
        for lf in &leaves {
            self.render_leaf(lf, now_ms, &insights);
        }

        // 4. Render every draggable Horizontal & Vertical Splitter
        for sp in &splitters {
            let active = matches!(self.drag_op, DragOp::Splitter { split_id, .. } if split_id == sp.split_id);
            let hov = self.is_hovered(sp.rect.x - 2.0, sp.rect.y - 2.0, sp.rect.w + 4.0, sp.rect.h + 4.0);
            self.draw.rect(
                sp.rect.x,
                sp.rect.y,
                sp.rect.w,
                sp.rect.h,
                if active || hov { col::FIRE_DIM } else { col::BG },
            );
            if sp.dir == SplitDir::Horizontal {
                self.draw.rect(sp.rect.x, sp.rect.y, 1.0, sp.rect.h, col::LINE);
                self.draw
                    .rect(sp.rect.x + sp.rect.w - 1.0, sp.rect.y, 1.0, sp.rect.h, col::LINE);
            } else {
                self.draw.rect(sp.rect.x, sp.rect.y, sp.rect.w, 1.0, col::LINE);
                self.draw
                    .rect(sp.rect.x, sp.rect.y + sp.rect.h - 1.0, sp.rect.w, 1.0, col::LINE);
            }
        }

        // 5. Z-Pass 2 Overlays: Drag-and-Drop 5-Zone Docking Preview, Layout Menu, or 2-Zone Hover Card!
        if let DragOp::TabDock { pane, .. } = self.drag_op {
            self.render_dock_overlay(pane, &leaves);
        } else if self.layout_menu_open {
            self.render_layout_menu(css_w);
        } else if self.drag_op == DragOp::None && self.mouse_inside {
            if let Some(target) = self.hover_target.clone() {
                self.render_hover_card(css_w, css_h, target);
            }
        }
    }

    fn render_topbar(&mut self, css_w: f32) {
        self.draw.rect(0.0, 0.0, css_w, TOPBAR_H, col::BG);
        self.draw.rect(0.0, TOPBAR_H - 1.0, css_w, 1.0, col::LINE);

        let mut x = 12.0;
        let cy = TOPBAR_H * 0.5;

        self.draw.icon(Icon::Flame, x, cy - 7.5, 15.0, col::FIRE);
        x += 18.0;
        x += self.draw.text(x, cy - 7.5, 12.5, col::INK1, "blaze") + 4.0;
        x += self.draw.text(x, cy - 7.5, 12.5, col::INK4, "/") + 4.0;
        x += self.draw.text(x, cy - 7.5, 12.5, col::INK2, "explore") + 14.0;

        let proc_txt = if self.world.pid > 0 {
            format!(
                "{} (pid {}{})",
                self.world.package,
                self.world.pid,
                if self.world.run_alive { "" } else { " · exit" }
            )
        } else {
            self.world.package.clone()
        };
        let pill_w = self.draw.measure_text(&proc_txt, 11.0) + 26.0;
        self.draw.card(x, 8.0, pill_w, 22.0, 5.0, col::BG2, col::LINE);
        self.draw.circle(
            x + 10.0,
            cy,
            3.2,
            if self.world.connected { col::OK } else { col::DANGER },
        );
        self.draw.text(x + 19.0, cy - 6.8, 11.0, col::INK1, &proc_txt);
        x += pill_w + 12.0;

        // Inline Live Metrics
        self.draw.rect(x, 10.0, 1.0, 18.0, col::LINE);
        x += 12.0;

        let max_left_x = css_w - 430.0;
        if let Some(b) = self.world.builds.first() {
            if b.reached > 0 && x < max_left_x {
                let pct = ((b.found as f64 / b.reached as f64) * 100.0).round() as u32;
                x += self.draw.text(x, cy - 6.8, 11.0, col::INK3, "cache ") + 1.0;
                x += self.draw.text(
                    x,
                    cy - 6.8,
                    11.0,
                    if pct >= 80 { col::OK } else { col::FIRE },
                    &format!("{pct}%"),
                ) + 12.0;
            }
        }
        if x < max_left_x {
            let ret = self.world.total_retained_bytes();
            x += self.draw.text(x, cy - 6.8, 11.0, col::INK3, "heap ") + 1.0;
            x += self
                .draw
                .text(x, cy - 6.8, 11.0, col::INK1, &fmt_bytes(ret as i64))
                + 12.0;
        }
        if x < max_left_x {
            let errs: u32 = self.world.builds.iter().map(|b| b.errors).sum();
            let warns: u32 = self.world.builds.iter().map(|b| b.warnings).sum();
            x += self.draw.text(
                x,
                cy - 6.8,
                11.0,
                if errs > 0 { col::DANGER } else { col::OK },
                &errs.to_string(),
            );
            x += self.draw.text(x, cy - 6.8, 11.0, col::INK3, " err · ");
            x += self.draw.text(
                x,
                cy - 6.8,
                11.0,
                if warns > 0 { col::FIRE } else { col::INK2 },
                &warns.to_string(),
            );
            x += self.draw.text(x, cy - 6.8, 11.0, col::INK3, " warn") + 12.0;
        }
        if x < max_left_x {
            self.draw
                .text(x, cy - 6.8, 11.0, col::INK2, &format!("{} ev", self.world.total_events));
        }

        // Right controls: symbol-first (`[Pan | Select]` + `−` `+` `Pause` `Clear` `Layout`)
        let mut rx = css_w - 12.0;

        rx -= 26.0;
        self.draw_icon_btn(rx, 8.0, 26.0, 22.0, Icon::Grid, self.layout_menu_open, Action::ToggleLayoutMenu);
        rx -= 5.0;

        rx -= 26.0;
        self.draw_icon_btn(rx, 8.0, 26.0, 22.0, Icon::Trash, false, Action::ClearTrace);
        rx -= 5.0;

        rx -= 26.0;
        self.draw_icon_btn(
            rx,
            8.0,
            26.0,
            22.0,
            if self.paused { Icon::Play } else { Icon::Pause },
            self.paused,
            Action::TogglePause,
        );
        rx -= 5.0;

        rx -= 26.0;
        self.draw_icon_btn(rx, 8.0, 26.0, 22.0, Icon::ZoomIn, false, Action::ZoomIn);
        rx -= 4.0;
        rx -= 26.0;
        self.draw_icon_btn(rx, 8.0, 26.0, 22.0, Icon::ZoomOut, false, Action::ZoomOut);
        rx -= 8.0;

        // Symbol-only `[Pan | Select]` segmented mode switch
        let seg_bw = 26.0;
        let seg_w = seg_bw * 2.0;
        rx -= seg_w;
        self.draw.card(rx, 8.0, seg_w, 22.0, 5.0, col::BG2, col::LINE);
        let pan_act = self.mode == DragMode::Pan;
        let sel_act = self.mode == DragMode::Select;
        if pan_act {
            self.draw.rrect(rx + 1.0, 9.0, seg_bw - 1.0, 20.0, 4.0, col::BG4);
        }
        if sel_act {
            self.draw
                .rrect(rx + seg_bw, 9.0, seg_bw - 1.0, 20.0, 4.0, col::FIRE_SOFT);
        }
        self.draw.rect(rx + seg_bw, 9.0, 1.0, 20.0, col::LINE);
        self.draw
            .icon(Icon::Pan, rx + 6.5, 12.5, 13.0, if pan_act { col::INK1 } else { col::INK3 });
        self.draw.icon(
            Icon::SelectRange,
            rx + seg_bw + 6.5,
            12.5,
            13.0,
            if sel_act { col::FIRE } else { col::INK3 },
        );
        self.push_hit(rx, 8.0, seg_bw, 22.0, Action::SetMode(DragMode::Pan), "pointer");
        self.push_hit(rx + seg_bw, 8.0, seg_bw, 22.0, Action::SetMode(DragMode::Select), "pointer");
    }

    fn draw_icon_btn(&mut self, x: f32, y: f32, w: f32, h: f32, icon: Icon, active: bool, action: Action) {
        let hov = self.is_hovered(x, y, w, h);
        let bg = if active {
            col::FIRE_SOFT
        } else if hov {
            col::BG3
        } else {
            col::BG2
        };
        let stroke = if active {
            col::FIRE
        } else if hov {
            col::LINE2
        } else {
            col::LINE
        };
        self.draw.card(x, y, w, h, 5.0, bg, stroke);
        let s = 13.0;
        self.draw.icon(
            icon,
            x + (w - s) * 0.5,
            y + (h - s) * 0.5,
            s,
            if active {
                col::FIRE
            } else if hov {
                col::INK1
            } else {
                col::INK2
            },
        );
        self.push_hit(x, y, w, h, action, "pointer");
    }

    fn draw_btn(&mut self, x: f32, y: f32, w: f32, h: f32, label: &str, active: bool, action: Action) {
        let hov = self.is_hovered(x, y, w, h);
        let bg = if active {
            col::FIRE_SOFT
        } else if hov {
            col::BG3
        } else {
            col::BG2
        };
        let stroke = if active {
            col::FIRE
        } else if hov {
            col::LINE2
        } else {
            col::LINE
        };
        self.draw.card(x, y, w, h, 5.0, bg, stroke);
        let tw = self.draw.measure_text(label, 10.5);
        self.draw.text(
            x + (w - tw) * 0.5,
            y + (h - 13.0) * 0.5,
            10.5,
            if active || hov { col::INK1 } else { col::INK2 },
            label,
        );
        self.push_hit(x, y, w, h, action, "pointer");
    }

    // -----------------------------------------------------------------------
    // Modular Dock Leaf Chrome (Draggable Tabs + Split `|` + Expand `[]` + Close `×`)
    // -----------------------------------------------------------------------

    fn render_leaf(&mut self, lf: &LeafLayout, now_ms: f64, insights: &[Insight]) {
        // Pane surface & header
        self.draw.rect(lf.rect.x, lf.rect.y, lf.rect.w, lf.rect.h, col::BG);
        self.draw
            .rect(lf.header.x, lf.header.y, lf.header.w, lf.header.h, [0.068, 0.068, 0.072, 1.0]);
        self.draw.rect(
            lf.header.x,
            lf.header.y + lf.header.h - 1.0,
            lf.header.w,
            1.0,
            col::LINE,
        );

        let Some(active_pane) = lf.active_pane() else {
            return;
        };

        // Right-side vector pane window controls: Split, Expand/Restore, Close
        let mut ctrl_x = lf.header.x + lf.header.w - 6.0;
        let btn_y = lf.header.y + 4.0;
        let btn_h = 20.0;

        ctrl_x -= 20.0;
        self.draw_pane_icon(ctrl_x, btn_y, 20.0, btn_h, Icon::Close, false, Action::ClosePane(active_pane));
        ctrl_x -= 3.0;

        let is_max = self.dock.maximized == Some(active_pane);
        ctrl_x -= 20.0;
        self.draw_pane_icon(
            ctrl_x,
            btn_y,
            20.0,
            btn_h,
            if is_max { Icon::Restore } else { Icon::Expand },
            is_max,
            Action::MaximizePane(active_pane),
        );
        ctrl_x -= 3.0;

        if lf.tabs.len() > 1 && !is_max {
            ctrl_x -= 20.0;
            self.draw_pane_icon(
                ctrl_x,
                btn_y,
                20.0,
                btn_h,
                Icon::Split,
                false,
                Action::PopOutPane(lf.id, active_pane),
            );
            ctrl_x -= 4.0;
        }

        // Draggable Symbol-First Tabs clipped cleanly before right-side window controls
        self.draw
            .push_clip(lf.header.x, lf.header.y, (ctrl_x - lf.header.x).max(20.0), lf.header.h);
        let mut tx = lf.header.x + 6.0;
        for (ti, &pane) in lf.tabs.iter().enumerate() {
            let active = ti == lf.active;
            let title = pane.title();
            let badge = if pane == PaneId::Advisor && !insights.is_empty() {
                Some(insights.len().to_string())
            } else {
                None
            };
            let ico_w = 16.0;
            let mut tw = ico_w + self.draw.measure_text(title, 10.5) + 15.0;
            if let Some(ref b) = badge {
                tw += self.draw.measure_text(b, 9.0) + 14.0;
            }
            let hov = self.is_hovered(tx, lf.header.y, tw, lf.header.h);
            if active {
                self.draw
                    .rect(tx, lf.header.y + 1.0, tw, lf.header.h - 2.0, [1.0, 1.0, 1.0, 0.03]);
                self.draw
                    .rect(tx + 5.0, lf.header.y + lf.header.h - 2.0, tw - 10.0, 2.0, col::FIRE);
            }
            let ico_col = if active {
                col::FIRE
            } else if hov {
                col::INK1
            } else {
                col::INK3
            };
            self.draw
                .icon(pane_icon(pane), tx + 7.0, lf.header.y + 8.0, 12.0, ico_col);
            self.draw.text(
                tx + 22.0,
                lf.header.y + 7.5,
                10.5,
                if active || hov { col::INK1 } else { col::INK3 },
                title,
            );
            if let Some(ref b) = badge {
                let bx = tx + 22.0 + self.draw.measure_text(title, 10.5) + 5.0;
                self.draw.badge(
                    bx,
                    lf.header.y + 6.0,
                    15.0,
                    9.0,
                    b,
                    col::FIRE,
                    col::FIRE_SOFT,
                    col::FIRE_DIM,
                );
            }
            self.push_hit(
                tx,
                lf.header.y,
                tw.min((ctrl_x - tx).max(0.0)),
                lf.header.h,
                Action::SelectTab(lf.id, ti, pane),
                "grab",
            );
            tx += tw + 2.0;
        }

        // Selection Range Pill on Timeline pane header
        if active_pane == PaneId::Timeline {
            if let Some((s0, s1)) = self.selection_ms {
                let lbl = format!("Range {}", fmt_span(s1 - s0));
                let lw = self.draw.measure_text(&lbl, 9.5);
                let zw = 34.0;
                let xw = 16.0;
                let pw = lw + zw + xw + 16.0;
                let px = tx + 6.0;
                if px + pw < ctrl_x {
                    self.draw
                        .card(px, lf.header.y + 4.0, pw, 20.0, 4.0, col::FIRE_SOFT, col::FIRE_DIM);
                    self.draw.text(px + 6.0, lf.header.y + 7.8, 9.5, col::INK1, &lbl);
                    let zx = px + lw + 10.0;
                    self.draw.rrect(zx, lf.header.y + 6.0, zw, 16.0, 3.0, col::FIRE);
                    self.draw.text(zx + 5.0, lf.header.y + 7.8, 9.0, col::WHITE, "Zoom");
                    self.push_hit(zx, lf.header.y + 4.0, zw, 20.0, Action::ZoomSelection, "pointer");

                    let xx = zx + zw + 3.0;
                    self.draw.text(xx + 3.0, lf.header.y + 7.2, 10.0, col::INK2, "×");
                    self.push_hit(xx, lf.header.y + 4.0, xw, 20.0, Action::ClearSelection, "pointer");
                }
            }
        }
        self.draw.pop_clip();

        // Active Pane Body (clipped cleanly to `lf.body`)
        self.draw.push_clip(lf.body.x, lf.body.y, lf.body.w, lf.body.h);
        match active_pane {
            PaneId::Overview => self.render_overview_pane(lf.body),
            PaneId::Timeline => self.render_timeline_pane(lf.body, now_ms),
            PaneId::FlameGraph => self.render_flame_pane(lf.body),
            PaneId::CodeMap => self.render_codemap_pane(lf.body),
            PaneId::CpuTopology => self.render_cpu_pane(lf.body),
            PaneId::HotSpots => self.render_hotspots_pane(lf.body),
            PaneId::HeapDiff => self.render_heap_pane(lf.body),
            PaneId::Builds => self.render_builds_pane(lf.body),
            PaneId::Advisor => self.render_advisor_pane(lf.body, insights),
            PaneId::IoLocks => self.render_io_pane(lf.body),
            PaneId::Inspector => self.render_inspector_pane(lf.body),
        }
        self.draw.pop_clip();
    }

    fn draw_pane_icon(&mut self, x: f32, y: f32, w: f32, h: f32, icon: Icon, active: bool, action: Action) {
        let hov = self.is_hovered(x, y, w, h);
        if active || hov {
            self.draw.card(
                x,
                y,
                w,
                h,
                4.0,
                if active { col::FIRE_SOFT } else { col::BG3 },
                if active { col::FIRE_DIM } else { col::LINE },
            );
        }
        let s = 11.5;
        self.draw.icon(
            icon,
            x + (w - s) * 0.5,
            y + (h - s) * 0.5,
            s,
            if active {
                col::FIRE
            } else if hov {
                col::INK1
            } else {
                col::INK3
            },
        );
        self.push_hit(x, y, w, h, action, "pointer");
    }

    fn draw_vscrollbar(&mut self, pane: PaneId, track_x: f32, track_y: f32, track_h: f32, content_h: f32) {
        let max_scroll = (content_h - track_h).max(0.0);
        self.pane_max_scroll.insert(pane, max_scroll);
        let cur = self.scroll_of(pane).clamp(0.0, max_scroll);
        self.pane_scroll.insert(pane, cur);
        if max_scroll <= 1.0 || track_h < 32.0 {
            return;
        }
        let track = Rect {
            x: track_x,
            y: track_y + 2.0,
            w: 8.0,
            h: (track_h - 4.0).max(16.0),
        };
        let thumb_h = ((track_h / content_h.max(1.0)) * track.h).clamp(24.0, track.h);
        let thumb_y = track.y + (cur / max_scroll) * (track.h - thumb_h);
        let thumb = Rect {
            x: track.x + 1.5,
            y: thumb_y,
            w: 5.0,
            h: thumb_h,
        };
        let dragging = matches!(self.drag_op, DragOp::VScroll { pane: p, .. } if p == pane);
        let hov = self.is_hovered(track.x - 2.0, track.y, track.w + 4.0, track.h);
        self.draw
            .rrect(track.x + 1.0, track.y, 6.0, track.h, 3.0, [0.09, 0.09, 0.10, 0.85]);
        self.draw.rrect(
            thumb.x,
            thumb.y,
            thumb.w,
            thumb.h,
            2.5,
            if dragging {
                col::FIRE
            } else if hov {
                col::INK2
            } else {
                [0.36, 0.36, 0.39, 0.85]
            },
        );
        self.vscrolls.push(VScrollHit {
            pane,
            track,
            thumb,
            max_scroll,
        });
    }

    // -----------------------------------------------------------------------
    // Pane: Vitals (Human-First Health Scorecard + Inline Alerts + Smooth Charts)
    // -----------------------------------------------------------------------

    fn render_overview_pane(&mut self, area: Rect) {
        let pad = 10.0;
        let gap = 8.0;
        let scroll = self.scroll_of(PaneId::Overview);
        let avail_w = (area.w - pad * 2.0 - 6.0).max(260.0);
        let x0 = area.x + pad;
        let mut y = area.y + 8.0 - scroll;

        // 1. 4-Pillar Health Scorecard (instant green / amber / red verdict + 1-click jump to culprit!)
        let tot_cpu = self.world.total_cpu_ns as f64;
        let tot_lock = self.world.total_lock_ns as f64;
        let tot_io = self.world.total_io_ns as f64;
        let tot_all = (tot_cpu + tot_lock + tot_io).max(1.0);
        let stall_pct = ((tot_lock + tot_io) / tot_all) * 100.0;

        // Top self-time hotspot
        let total_self: u64 = self
            .world
            .fns
            .iter()
            .map(|(_, f)| f.total_ns.saturating_sub(f.child_ns))
            .sum::<u64>()
            .max(1);
        let hottest = self
            .world
            .fns
            .iter()
            .max_by_key(|(_, f)| f.total_ns.saturating_sub(f.child_ns))
            .map(|(_, f)| {
                let s = f.total_ns.saturating_sub(f.child_ns);
                (f.slot, f.name.clone(), (s as f64 / total_self as f64) * 100.0)
            });

        let ret_bytes = self.world.total_retained_bytes();
        let mem_slope = self.world.heap_slope_bps();
        let mem_growing = mem_slope > 16_384.0;
        let mem_sev = if mem_slope > 96_000.0 || ret_bytes > 16 * 1024 * 1024 {
            2
        } else if mem_growing || ret_bytes > 4 * 1024 * 1024 {
            1
        } else {
            0
        };

        let speed_sev = if let Some((_, _, pct)) = &hottest {
            if *pct >= 40.0 {
                2
            } else if *pct >= 24.0 {
                1
            } else {
                0
            }
        } else {
            0
        };
        let stall_sev = if stall_pct >= 15.0 {
            2
        } else if stall_pct >= 5.0 {
            1
        } else {
            0
        };
        let migs = self.world.total_migrations;
        let core_sev = if migs >= 64 {
            2
        } else if migs >= 12 {
            1
        } else {
            0
        };

        let score_cols = if avail_w >= 620.0 { 4 } else { 2 };
        let sc_w = (avail_w - gap * (score_cols as f32 - 1.0)) / score_cols as f32;
        let sc_h = 52.0;

        let (speed_val, speed_sub, speed_act) = if let Some((slot, name, pct)) = hottest {
            (format!("{pct:.0}% hot"), name, Action::InspectSlot(slot))
        } else {
            ("OK".into(), "balanced".into(), Action::OpenPane(PaneId::HotSpots))
        };
        let mem_sub = if mem_slope.abs() >= 1024.0 {
            format!(
                "{}{}/s",
                if mem_slope > 0.0 { "+" } else { "" },
                fmt_bytes(mem_slope as i64)
            )
        } else {
            "stable".into()
        };
        let stall_holder = self
            .world
            .io_rows
            .iter()
            .rev()
            .find(|r| r.kind == "sync_wait" && r.holder_thread > 0)
            .map(|r| format!("mutex · t#{}", r.holder_thread))
            .unwrap_or_else(|| "clean".into());

        let pillars = [
            (Icon::Flame, "HOTSPOT", speed_val, speed_sub, speed_sev, speed_act),
            (
                Icon::Memory,
                "MEMORY",
                fmt_bytes(ret_bytes as i64),
                mem_sub,
                mem_sev,
                Action::OpenPane(PaneId::HeapDiff),
            ),
            (
                Icon::Lock,
                "STALLS",
                format!("{stall_pct:.0}% wait"),
                stall_holder,
                stall_sev,
                Action::OpenPane(PaneId::IoLocks),
            ),
            (
                Icon::Cpu,
                "CORES",
                format!("{migs} hops"),
                format!("{}P+{}E", self.world.topology.p_cores, self.world.topology.e_cores),
                core_sev,
                Action::OpenPane(PaneId::CpuTopology),
            ),
        ];

        for (i, (ico, lbl, val, sub, sev, act)) in pillars.into_iter().enumerate() {
            let cx = x0 + (i % score_cols) as f32 * (sc_w + gap);
            let cy = y + (i / score_cols) as f32 * (sc_h + gap);
            let sc = match sev {
                2 => col::DANGER,
                1 => col::WARN,
                _ => col::OK,
            };
            let hov = self.is_hovered(cx, cy, sc_w, sc_h);
            self.draw.card(
                cx,
                cy,
                sc_w,
                sc_h,
                7.0,
                if hov { col::CARD_HOV } else { col::BG2 },
                if hov { sc } else if sev == 2 { col::with_alpha(sc, 0.5) } else { col::LINE },
            );
            self.draw.icon(ico, cx + 10.0, cy + 8.0, 11.5, sc);
            self.draw.text(cx + 25.0, cy + 7.5, 9.0, col::INK3, lbl);
            self.draw.circle(cx + sc_w - 11.0, cy + 13.0, 3.2, sc);
            self.draw
                .text_max(cx + 10.0, cy + 21.0, 12.0, col::INK1, &val, sc_w - 20.0);
            self.draw
                .text_max(cx + 10.0, cy + 37.0, 9.5, if sev > 0 { sc } else { col::INK3 }, &sub, sc_w - 20.0);
            self.push_hit(cx, cy, sc_w, sc_h, act, "pointer");
        }
        let sc_rows = (4 + score_cols - 1) / score_cols;
        y += sc_rows as f32 * (sc_h + gap);

        // 2. Inline Actionable Alert Pills (surfaces Alerts directly at the top, clickable!)
        let insights = self.world.analyze_insights();
        if !insights.is_empty() {
            let mut ax = x0;
            let pill_h = 22.0;
            for ins in insights.iter().take(4) {
                let (fg, bg) = match ins.severity {
                    2 => (col::DANGER, col::DANGER_SOFT),
                    1 => (col::FIRE, col::FIRE_SOFT),
                    _ => (col::OK, col::OK_SOFT),
                };
                let ico = match ins.category {
                    "HOT" => Icon::Flame,
                    "LOCK" => Icon::Lock,
                    "HEAP" => Icon::Memory,
                    "CPU" => Icon::Cpu,
                    _ => Icon::Layers,
                };
                let txt = format!("{} · {}", ins.title, ins.metric);
                let pw = (24.0 + self.draw.measure_text(&txt, 9.8) + 10.0).min(avail_w);
                if ax + pw > x0 + avail_w {
                    break;
                }
                let hov = self.is_hovered(ax, y, pw, pill_h);
                self.draw.card(
                    ax,
                    y,
                    pw,
                    pill_h,
                    5.0,
                    if hov { col::BG3 } else { bg },
                    col::with_alpha(fg, if hov { 0.85 } else { 0.45 }),
                );
                self.draw.icon(ico, ax + 6.0, y + 5.0, 11.5, fg);
                self.draw
                    .text_max(ax + 21.0, y + 4.8, 9.8, col::INK1, &txt, pw - 27.0);
                let target_pane = match ins.target_tab {
                    2 => PaneId::CpuTopology,
                    3 => PaneId::HotSpots,
                    4 => PaneId::HeapDiff,
                    5 => PaneId::Builds,
                    _ => PaneId::IoLocks,
                };
                self.push_hit(ax, y, pw, pill_h, Action::OpenPane(target_pane), "pointer");
                ax += pw + 6.0;
            }
            y += pill_h + gap;
        }

        // 3. 2x2 Grid of Clean, Non-Redundant Time-Series Signal Charts (height depends ONLY on area.h, NEVER on scroll!)
        let cols = if avail_w >= 540.0 { 2 } else { 1 };
        let rows = if cols == 2 { 2 } else { 4 };
        let cw = (avail_w - gap * (cols as f32 - 1.0)) / cols as f32;
        let header_block_h = (y + scroll) - area.y;
        let avail_h = (area.h - header_block_h - 10.0).max(180.0);
        let ch = ((avail_h - gap * (rows as f32 - 1.0)) / rows as f32).clamp(108.0, 210.0);

        let buckets = self.world.vitals.clone();
        let n = buckets.len();
        let origin = self.world.trace_origin_ms.unwrap_or_else(|| {
            buckets.first().map(|b| b.ts_ms).unwrap_or(0.0)
        });

        let mut hover_idx: Option<usize> = None;
        for ci in 0..4 {
            let cx = x0 + (ci % cols) as f32 * (cw + gap);
            let cy = y + (ci / cols) as f32 * (ch + gap);
            let px = cx + 10.0;
            let py = cy + 28.0;
            let pw = (cw - 20.0).max(40.0);
            let ph = (ch - 38.0).max(26.0);
            if n > 0 && self.is_hovered(px, py, pw, ph) {
                let frac = ((self.mouse_x - px) / pw).clamp(0.0, 0.9999);
                hover_idx = Some(((frac * n as f32) as usize).min(n - 1));
                break;
            }
        }

        let thr_cap_ns = (self.world.threads.len().max(1) as f64) * 500_000_000.0;

        // Smooth 3-point moving average on CPU % to eliminate 500ms bucket-boundary aliasing teeth!
        let raw_cpu: Vec<f64> = buckets
            .iter()
            .map(|b| ((b.cpu_ns as f64 / thr_cap_ns) * 100.0).min(100.0))
            .collect();
        let cpu_vals: Vec<f64> = (0..raw_cpu.len())
            .map(|i| {
                let a = raw_cpu[i.saturating_sub(1)];
                let b = raw_cpu[i];
                let c = raw_cpu[(i + 1).min(raw_cpu.len() - 1)];
                (a + b * 2.0 + c) * 0.25
            })
            .collect();
        let cpu_cur = hover_idx
            .and_then(|i| cpu_vals.get(i))
            .or_else(|| cpu_vals.last())
            .copied()
            .unwrap_or(0.0);
        self.draw_line_chart(
            Rect { x: x0, y, w: cw, h: ch },
            Icon::Cpu,
            "CPU LOAD",
            &format!("{cpu_cur:.0}%"),
            &format!("{} thr", self.world.threads.len().max(1)),
            col::FIRE,
            &cpu_vals,
            100.0,
            &buckets,
            origin,
            hover_idx,
        );

        let mem_vals: Vec<f64> = buckets.iter().map(|b| b.retained_bytes as f64).collect();
        let mem_max = mem_vals.iter().copied().fold(65536.0_f64, f64::max) * 1.15;
        let mem_cur = hover_idx
            .and_then(|i| mem_vals.get(i))
            .or_else(|| mem_vals.last())
            .copied()
            .unwrap_or(ret_bytes as f64);
        self.draw_line_chart(
            Rect {
                x: x0 + (1 % cols) as f32 * (cw + gap),
                y: y + (1 / cols) as f32 * (ch + gap),
                w: cw,
                h: ch,
            },
            Icon::Memory,
            "RETAINED HEAP",
            &fmt_bytes(mem_cur as i64),
            &format!("{} live", self.world.live_ptrs.len()),
            col::SERIES2,
            &mem_vals,
            mem_max,
            &buckets,
            origin,
            hover_idx,
        );

        // Chart 2: Root Span Task Duration (explicitly explains what it measures!)
        self.draw_latency_chart(
            Rect {
                x: x0 + (2 % cols) as f32 * (cw + gap),
                y: y + (2 / cols) as f32 * (ch + gap),
                w: cw,
                h: ch,
            },
            &buckets,
            origin,
            hover_idx,
        );

        // Chart 3: Discrete Lock & I/O Stalls (pure bars — no redundant line!)
        self.draw_stall_chart(
            Rect {
                x: x0 + (3 % cols) as f32 * (cw + gap),
                y: y + (3 / cols) as f32 * (ch + gap),
                w: cw,
                h: ch,
            },
            &buckets,
            origin,
            hover_idx,
        );

        let content_h = (y + scroll - area.y) + rows as f32 * (ch + gap) + 8.0;
        self.draw_vscrollbar(PaneId::Overview, area.x + area.w - 9.0, area.y, area.h, content_h);
    }

    /// Clean continuous line chart with soft translucent wash (no chunky redundant bars!).
    #[allow(clippy::too_many_arguments)]
    fn draw_line_chart(
        &mut self,
        r: Rect,
        icon: Icon,
        title: &str,
        primary: &str,
        sub: &str,
        color: col::Rgba,
        vals: &[f64],
        max_v: f64,
        buckets: &[crate::model::VitalsBucket],
        origin: f64,
        hover_idx: Option<usize>,
    ) {
        self.draw.card(r.x, r.y, r.w, r.h, 7.0, col::BG2, col::LINE);
        self.draw.icon(icon, r.x + 10.0, r.y + 8.0, 12.0, color);
        self.draw.text(r.x + 26.0, r.y + 7.5, 10.0, col::INK3, title);

        let pw_txt = self.draw.measure_text(primary, 11.5);
        let sw_txt = self.draw.measure_text(sub, 9.5);
        self.draw
            .text(r.x + r.w - pw_txt - sw_txt - 18.0, r.y + 6.5, 11.5, col::INK1, primary);
        self.draw
            .text(r.x + r.w - sw_txt - 10.0, r.y + 8.0, 9.5, col::INK3, sub);

        let px = r.x + 10.0;
        let py = r.y + 28.0;
        let pw = (r.w - 20.0).max(40.0);
        let ph = (r.h - 38.0).max(26.0);
        for g in [0.0, 0.5, 1.0] {
            self.draw
                .rect(px, py + ph * g, pw, 1.0, [0.16, 0.16, 0.18, 0.5]);
        }

        let n = vals.len();
        if n == 0 {
            return;
        }
        let step_x = if n > 1 { pw / (n - 1) as f32 } else { pw };
        let wash = col::with_alpha(color, 0.11);

        let mut prev: Option<(f32, f32)> = None;
        for (i, &v) in vals.iter().enumerate() {
            let frac = (v / max_v.max(1e-6)).clamp(0.0, 1.0) as f32;
            let cx = if n > 1 { px + i as f32 * step_x } else { px + pw * 0.5 };
            let cy = py + ph * (1.0 - frac);
            if let Some((lx, ly)) = prev {
                let seg_w = (cx - lx).max(1.0);
                let mid_y = (ly + cy) * 0.5;
                self.draw.rect(lx, mid_y, seg_w, (py + ph - mid_y).max(0.0), wash);
                self.draw.line(lx, ly, cx, cy, 1.8, color);
            }
            prev = Some((cx, cy));
        }

        if let Some(hi) = hover_idx {
            if let Some(&v) = vals.get(hi) {
                let frac = (v / max_v.max(1e-6)).clamp(0.0, 1.0) as f32;
                let hx = if n > 1 { px + hi as f32 * step_x } else { px + pw * 0.5 };
                let hy = py + ph * (1.0 - frac);
                self.draw.rect(hx, py, 1.0, ph, [0.92, 0.91, 0.89, 0.35]);
                self.draw.circle(hx, hy, 3.2, color);
                if let Some(b) = buckets.get(hi) {
                    let t_lbl = fmt_elapsed_tick(b.ts_ms - origin, 100.0);
                    self.draw
                        .text(r.x + 114.0, r.y + 8.0, 9.5, col::INK3, &format!("@ {t_lbl}"));
                    if self.is_hovered(px, py, pw, ph) {
                        self.push_hit(px, py, pw, ph, Action::JumpToTraceTime(b.ts_ms), "pointer");
                    }
                }
            }
        }
    }

    fn draw_latency_chart(
        &mut self,
        r: Rect,
        buckets: &[crate::model::VitalsBucket],
        origin: f64,
        hover_idx: Option<usize>,
    ) {
        self.draw.card(r.x, r.y, r.w, r.h, 7.0, col::BG2, col::LINE);
        self.draw.icon(Icon::Pulse, r.x + 10.0, r.y + 8.0, 12.0, col::OK);
        self.draw.text(r.x + 26.0, r.y + 7.5, 10.0, col::INK3, "ROOT SPANS");

        let cur = hover_idx.and_then(|i| buckets.get(i)).or_else(|| buckets.last());
        if let Some(b) = cur {
            let p95_s = format!("P95 {}", fmt_ns(b.p95_ns));
            let p50_s = format!("P50 {}", fmt_ns(b.p50_ns));
            let w95 = self.draw.measure_text(&p95_s, 9.5);
            let w50 = self.draw.measure_text(&p50_s, 9.5);
            let mut rx = r.x + r.w - 10.0;
            rx -= w95;
            self.draw.text(rx, r.y + 8.0, 9.5, col::FIRE, &p95_s);
            rx -= w50 + 10.0;
            self.draw.text(rx, r.y + 8.0, 9.5, col::OK, &p50_s);
        }

        let px = r.x + 10.0;
        let py = r.y + 28.0;
        let pw = (r.w - 20.0).max(40.0);
        let ph = (r.h - 38.0).max(26.0);
        for g in [0.0, 0.5, 1.0] {
            self.draw
                .rect(px, py + ph * g, pw, 1.0, [0.16, 0.16, 0.18, 0.5]);
        }

        let n = buckets.len();
        if n == 0 {
            return;
        }
        let max_ns = buckets
            .iter()
            .map(|b| b.p95_ns.max(b.p50_ns))
            .max()
            .unwrap_or(1_000_000)
            .max(1_000_000) as f64
            * 1.15;
        let step_x = if n > 1 { pw / (n - 1) as f32 } else { pw };

        // Clean 2-series root-task envelope: emerald P50 (typical) + fire P95 (tail)
        let series: [(col::Rgba, fn(&crate::model::VitalsBucket) -> u64); 2] = [
            (col::OK, |b| b.p50_ns),
            (col::FIRE, |b| b.p95_ns),
        ];
        for (c, get) in series {
            let mut prev: Option<(f32, f32)> = None;
            for (i, b) in buckets.iter().enumerate() {
                let frac = (get(b) as f64 / max_ns).clamp(0.0, 1.0) as f32;
                let cx = if n > 1 { px + i as f32 * step_x } else { px + pw * 0.5 };
                let cy = py + ph * (1.0 - frac);
                if let Some((lx, ly)) = prev {
                    self.draw.line(lx, ly, cx, cy, 1.7, c);
                }
                prev = Some((cx, cy));
            }
        }

        if let Some(hi) = hover_idx {
            if let Some(b) = buckets.get(hi) {
                let hx = if n > 1 { px + hi as f32 * step_x } else { px + pw * 0.5 };
                self.draw.rect(hx, py, 1.0, ph, [0.92, 0.91, 0.89, 0.35]);
                for (c, get) in series {
                    let frac = (get(b) as f64 / max_ns).clamp(0.0, 1.0) as f32;
                    self.draw.circle(hx, py + ph * (1.0 - frac), 2.8, c);
                }
                let t_lbl = fmt_elapsed_tick(b.ts_ms - origin, 100.0);
                self.draw
                    .text(r.x + 114.0, r.y + 8.0, 9.5, col::INK3, &format!("@ {t_lbl}"));
                if self.is_hovered(px, py, pw, ph) {
                    self.push_hit(px, py, pw, ph, Action::JumpToTraceTime(b.ts_ms), "pointer");
                }
            }
        }
    }

    fn draw_stall_chart(
        &mut self,
        r: Rect,
        buckets: &[crate::model::VitalsBucket],
        origin: f64,
        hover_idx: Option<usize>,
    ) {
        self.draw.card(r.x, r.y, r.w, r.h, 7.0, col::BG2, col::LINE);
        self.draw.icon(Icon::Lock, r.x + 10.0, r.y + 8.0, 12.0, col::DANGER);
        self.draw.text(r.x + 26.0, r.y + 7.5, 10.0, col::INK3, "LOCKS & I/O");

        let cur = hover_idx.and_then(|i| buckets.get(i)).or_else(|| buckets.last());
        if let Some(b) = cur {
            let io_s = format!("I/O {}", fmt_ns(b.io_ns));
            let lk_s = format!("Lock {}", fmt_ns(b.lock_ns));
            let wio = self.draw.measure_text(&io_s, 9.5);
            let wlk = self.draw.measure_text(&lk_s, 9.5);
            self.draw
                .text(r.x + r.w - wio - 10.0, r.y + 8.0, 9.5, col::WARN, &io_s);
            self.draw
                .text(r.x + r.w - wio - wlk - 20.0, r.y + 8.0, 9.5, col::DANGER, &lk_s);
        }

        let px = r.x + 10.0;
        let py = r.y + 28.0;
        let pw = (r.w - 20.0).max(40.0);
        let ph = (r.h - 38.0).max(26.0);
        for g in [0.0, 0.5, 1.0] {
            self.draw
                .rect(px, py + ph * g, pw, 1.0, [0.16, 0.16, 0.18, 0.55]);
        }

        let n = buckets.len();
        if n == 0 {
            return;
        }
        let max_ns = buckets
            .iter()
            .map(|b| b.lock_ns.saturating_add(b.io_ns))
            .max()
            .unwrap_or(1_000_000)
            .max(1_000_000) as f64
            * 1.15;
        let slot_w = pw / n.max(1) as f32;
        let bw = (slot_w - 1.5).clamp(2.0, 12.0);

        for (i, b) in buckets.iter().enumerate() {
            let bx = px + i as f32 * slot_w + (slot_w - bw) * 0.5;
            let h_lock = ((b.lock_ns as f64 / max_ns).clamp(0.0, 1.0) as f32) * ph;
            let h_io = ((b.io_ns as f64 / max_ns).clamp(0.0, 1.0) as f32) * ph;
            if h_lock > 0.5 {
                self.draw
                    .rrect(bx, py + ph - h_lock, bw, h_lock, 1.5, col::DANGER);
            }
            if h_io > 0.5 {
                self.draw
                    .rrect(bx, py + ph - h_lock - h_io, bw, h_io, 1.5, col::WARN);
            }
        }

        if let Some(hi) = hover_idx {
            if let Some(b) = buckets.get(hi) {
                let hx = px + (hi as f32 + 0.5) * slot_w;
                self.draw.rect(hx, py, 1.0, ph, [0.92, 0.91, 0.89, 0.35]);
                let t_lbl = fmt_elapsed_tick(b.ts_ms - origin, 100.0);
                self.draw
                    .text(r.x + 114.0, r.y + 8.0, 9.5, col::INK3, &format!("@ {t_lbl}"));
                if self.is_hovered(px, py, pw, ph) {
                    self.push_hit(px, py, pw, ph, Action::JumpToTraceTime(b.ts_ms), "pointer");
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Pane: Timeline (Interactive Overview Minimap + Fixed Elapsed Ruler + Scrollbar)
    // -----------------------------------------------------------------------

    fn render_timeline_pane(&mut self, area: Rect, now_ms: f64) {
        let mm_y = area.y;
        let ruler_y = area.y + MINIMAP_H;
        let stage_y = ruler_y + RULER_H;
        let stage_h = (area.h - MINIMAP_H - RULER_H).max(40.0);
        let plot_x = area.x + GUTTER_W;
        let plot_w = (area.w - GUTTER_W - 10.0).max(1.0);
        let live_edge = self.playhead_edge(now_ms);
        let view_start = self.view_end_ms - self.view_span_ms;
        let origin = self.world.trace_origin_ms.unwrap_or(view_start);

        // 1. Interactive Overview Minimap (full trace history + draggable viewport window & zoom handles!)
        self.draw.rect(area.x, mm_y, area.w, MINIMAP_H, [0.058, 0.058, 0.062, 1.0]);
        self.draw
            .rect(area.x, mm_y + MINIMAP_H - 1.0, area.w, 1.0, col::LINE);
        self.draw.rect(plot_x - 1.0, mm_y, 1.0, MINIMAP_H, col::LINE);

        let t_min = origin;
        let t_max = live_edge.max(t_min + self.view_span_ms).max(t_min + 1000.0);
        let total_ms = (t_max - t_min).max(1.0);

        // Left corner shows total recorded duration
        self.draw.circle(
            area.x + 12.0,
            mm_y + MINIMAP_H * 0.5,
            2.6,
            if self.following { col::FIRE } else { col::INK4 },
        );
        self.draw
            .text(area.x + 20.0, mm_y + 4.5, 9.5, col::INK3, &fmt_span(total_ms));

        // Sparkline activity density across the entire run (up to 160 bins)
        let bins = (plot_w / 4.0).floor().clamp(24.0, 160.0) as usize;
        let mut span_bins = [0u16; 160];
        let mut lock_bins = [false; 160];
        let mut heap_bins = [false; 160];
        let mut max_bin = 1u16;
        for bar in &self.world.bars {
            let b = (((bar.end_ms - t_min) / total_ms) * bins as f64)
                .floor()
                .clamp(0.0, (bins - 1) as f64) as usize;
            match bar.kind {
                BarKind::Span => {
                    span_bins[b] = span_bins[b].saturating_add(1);
                    if span_bins[b] > max_bin {
                        max_bin = span_bins[b];
                    }
                }
                BarKind::Lock => lock_bins[b] = true,
                BarKind::Heap => heap_bins[b] = true,
                BarKind::Io => {
                    span_bins[b] = span_bins[b].saturating_add(1);
                }
            }
        }
        let bin_w = plot_w / bins as f32;
        for i in 0..bins {
            let bx = plot_x + i as f32 * bin_w;
            if span_bins[i] > 0 {
                let h = ((span_bins[i] as f32 / max_bin as f32) * (MINIMAP_H - 7.0)).max(2.5);
                self.draw.rect(
                    bx,
                    mm_y + MINIMAP_H - 2.0 - h,
                    (bin_w - 0.8).max(1.0),
                    h,
                    [0.85, 0.34, 0.14, 0.45],
                );
            }
            if lock_bins[i] {
                self.draw
                    .rect(bx, mm_y + 2.0, (bin_w - 0.5).max(1.5), 3.0, col::DANGER);
            } else if heap_bins[i] {
                self.draw.rect(
                    bx,
                    mm_y + MINIMAP_H - 3.5,
                    (bin_w - 0.5).max(1.0),
                    2.0,
                    col::SERIES2,
                );
            }
        }

        // Viewport window overlay (`[view_start .. view_end_ms]`) + dim masks + edge handles
        let wx0 = plot_x + (((view_start - t_min) / total_ms).clamp(0.0, 1.0) as f32) * plot_w;
        let wx1 = plot_x + (((self.view_end_ms - t_min) / total_ms).clamp(0.0, 1.0) as f32) * plot_w;
        let win_x0 = wx0.min(wx1 - 10.0).max(plot_x);
        let win_x1 = wx1.max(win_x0 + 10.0).min(plot_x + plot_w);
        if win_x0 > plot_x {
            self.draw
                .rect(plot_x, mm_y + 1.0, win_x0 - plot_x, MINIMAP_H - 2.0, [0.0, 0.0, 0.0, 0.48]);
        }
        if win_x1 < plot_x + plot_w {
            self.draw.rect(
                win_x1,
                mm_y + 1.0,
                plot_x + plot_w - win_x1,
                MINIMAP_H - 2.0,
                [0.0, 0.0, 0.0, 0.48],
            );
        }
        let mm_drag = matches!(self.drag_op, DragOp::MinimapScrub { .. });
        self.draw.rrect(
            win_x0,
            mm_y + 1.0,
            win_x1 - win_x0,
            MINIMAP_H - 3.0,
            2.5,
            [0.92, 0.91, 0.89, if mm_drag { 0.14 } else { 0.07 }],
        );
        self.draw.border(
            win_x0,
            mm_y + 1.0,
            win_x1 - win_x0,
            MINIMAP_H - 3.0,
            2.5,
            1.0,
            if mm_drag { col::FIRE } else { col::INK2 },
        );
        // Crisp left & right zoom handles
        self.draw
            .rrect(win_x0 - 1.5, mm_y + 4.0, 3.0, MINIMAP_H - 9.0, 1.5, col::FIRE);
        self.draw
            .rrect(win_x1 - 1.5, mm_y + 4.0, 3.0, MINIMAP_H - 9.0, 1.5, col::FIRE);

        self.minimap_hit = Some(MinimapHit {
            bar: Rect {
                x: plot_x,
                y: mm_y,
                w: plot_w,
                h: MINIMAP_H,
            },
            win_x0,
            win_x1,
            t_min,
            t_max,
        });

        // 2. Ruler bar
        self.draw.rect(area.x, ruler_y, area.w, RULER_H, col::BG);
        self.draw
            .rect(area.x, ruler_y + RULER_H - 1.0, area.w, 1.0, col::LINE);
        self.draw.rect(plot_x - 1.0, ruler_y, 1.0, RULER_H, col::LINE);
        self.draw
            .text(area.x + 10.0, ruler_y + 4.5, 10.5, col::INK1, &fmt_span(self.view_span_ms));

        // Stage bg
        self.draw.rect(area.x, stage_y, area.w, stage_h, col::BG2);

        // Fixed trace-elapsed time ruler (anchored to `trace_origin_ms`, ZERO flicker or drift!)
        let rel_start = view_start - origin;
        let rel_end = self.view_end_ms - origin;
        let step_ms = nice_step_ms(self.view_span_ms, plot_w);
        let minor_ms = step_ms * 0.2;

        let mut minor = (rel_start / minor_ms).ceil() * minor_ms;
        let mut guard = 0;
        while minor <= rel_end && guard < 120 {
            let norm = (minor - rel_start) / self.view_span_ms;
            if norm > 0.005 && norm < 0.995 {
                let gx = (plot_x + (norm as f32) * plot_w).round();
                self.draw
                    .rect(gx, ruler_y + RULER_H - 5.0, 1.0, 4.0, col::LINE2);
            }
            minor += minor_ms;
            guard += 1;
        }

        let mut tick = (rel_start / step_ms).ceil() * step_ms;
        let mut mguard = 0;
        while tick <= rel_end && mguard < 32 {
            let norm = (tick - rel_start) / self.view_span_ms;
            if norm > 0.015 && norm < 0.985 {
                let gx = (plot_x + (norm as f32) * plot_w).round();
                let lbl = fmt_elapsed_tick(tick, step_ms);
                let tw = self.draw.measure_text(&lbl, 9.5);
                self.draw
                    .rect(gx, ruler_y + RULER_H - 9.0, 1.0, 8.0, col::INK4);
                if gx - tw * 0.5 > plot_x + 4.0 && gx + tw * 0.5 < area.x + area.w - 4.0 {
                    self.draw.text(gx - tw * 0.5, ruler_y + 3.5, 9.5, col::INK3, &lbl);
                }
                self.draw
                    .rect(gx, stage_y, 1.0, stage_h, [0.15, 0.15, 0.165, 0.55]);
            }
            tick += step_ms;
            mguard += 1;
        }

        // Build tracks from ECS Thread entities
        let mut tids: Vec<u32> = self.world.threads.iter().map(|(_, t)| t.tid).collect();
        tids.sort_unstable();
        if tids.is_empty() {
            tids.push(1);
        }

        let heap_h = 36.0;
        let io_h = 36.0;
        let avail = (stage_h - heap_h - io_h).max(60.0);
        let per_thread = (avail / tids.len() as f32).clamp(56.0, 92.0);

        struct Track {
            tid: Option<u32>,
            kind: u8,
            title: String,
            accent: col::Rgba,
            y_top: f32,
            height: f32,
            bar_h: f32,
            max_depth: u8,
        }

        let mut tracks = Vec::with_capacity(tids.len() + 2);
        let mut cur_y = 0.0;
        for &tid in &tids {
            let max_depth = if per_thread >= 74.0 { 3 } else { 2 };
            let bar_h = (((per_thread - 12.0) / max_depth as f32) - 2.0).clamp(18.0, 22.0);
            tracks.push(Track {
                tid: Some(tid),
                kind: 0,
                title: format!("Thread #{tid}"),
                accent: col::FIRE,
                y_top: cur_y,
                height: per_thread,
                bar_h,
                max_depth,
            });
            cur_y += per_thread;
        }
        tracks.push(Track {
            tid: None,
            kind: 1,
            title: "Heap".into(),
            accent: col::SERIES2,
            y_top: cur_y,
            height: heap_h,
            bar_h: 22.0,
            max_depth: 1,
        });
        cur_y += heap_h;
        let io_final_h = io_h.max(stage_h - cur_y);
        tracks.push(Track {
            tid: None,
            kind: 2,
            title: "Locks / I/O".into(),
            accent: col::DANGER,
            y_top: cur_y,
            height: io_final_h,
            bar_h: 22.0,
            max_depth: 1,
        });
        cur_y += io_final_h;

        let max_scroll = (cur_y - stage_h).max(0.0);
        self.pane_max_scroll.insert(PaneId::Timeline, max_scroll);
        let scroll_y = self.scroll_of(PaneId::Timeline).clamp(0.0, max_scroll);
        self.pane_scroll.insert(PaneId::Timeline, scroll_y);

        self.draw.push_clip(area.x, stage_y, area.w, stage_h);

        let mut hovered_gutter: Option<u32> = None;
        for (i, tr) in tracks.iter().enumerate() {
            let sy = stage_y + tr.y_top - scroll_y;
            if i % 2 == 1 {
                self.draw
                    .rect(plot_x, sy, plot_w, tr.height, [0.092, 0.092, 0.098, 1.0]);
            }
            if i > 0 {
                self.draw.rect(area.x, sy, area.w, 1.0, col::LINE);
            }
            if let Some(tid) = tr.tid {
                if self.mouse_inside
                    && self.mouse_x >= area.x
                    && self.mouse_x < plot_x
                    && self.mouse_y >= sy.max(stage_y)
                    && self.mouse_y <= (sy + tr.height).min(stage_y + stage_h)
                {
                    hovered_gutter = Some(tid);
                }
            }
        }

        // Left gutter
        self.draw
            .rect(area.x, stage_y, GUTTER_W, stage_h, [0.068, 0.068, 0.072, 1.0]);
        if let Some(htid) = hovered_gutter {
            if let Some(tr) = tracks.iter().find(|t| t.tid == Some(htid)) {
                self.draw
                    .rect(area.x, stage_y + tr.y_top - scroll_y, GUTTER_W - 1.0, tr.height, col::BG3);
            }
            self.hover_target = Some(HoverTarget::ThreadGutter(htid));
        }
        self.draw.rect(plot_x - 1.0, stage_y, 1.0, stage_h, col::LINE);

        for tr in &tracks {
            let sy = stage_y + tr.y_top - scroll_y;
            if sy + tr.height < stage_y || sy > stage_y + stage_h {
                continue;
            }
            let cy = sy + (tr.height * 0.5).min(20.0);
            self.draw.rrect(area.x + 10.0, cy - 2.5, 5.0, 5.0, 1.5, tr.accent);
            self.draw.text(
                area.x + 20.0,
                cy - 6.5,
                10.5,
                if tr.tid.is_some() && tr.tid == hovered_gutter {
                    col::INK1
                } else {
                    col::INK2
                },
                &tr.title,
            );
            if let Some(tid) = tr.tid {
                if let Some(&te) = self.world.thread_entity.get(&tid) {
                    if let Some(tc) = self.world.threads.get(te) {
                        if tc.last_cpu != CPU_UNKNOWN && tr.height >= 44.0 {
                            let is_e = (tc.last_flags & 0x10) != 0;
                            self.draw.text(
                                area.x + 20.0,
                                cy + 8.0,
                                9.5,
                                if is_e { col::SERIES2 } else { col::INK3 },
                                &format!("C{} · {}", tc.last_cpu, if is_e { "E" } else { "P" }),
                            );
                        }
                    }
                }
            }
        }

        // Plot bars
        self.draw.push_clip(plot_x, stage_y, plot_w, stage_h);

        let mut best_hover: Option<(f32, TimelineBar)> = None;
        let mut lock_arrows: Vec<(f32, f32, f32, bool)> = Vec::new();
        let mut row_last_x1: HashMap<(usize, u8), f32> = HashMap::new();

        for bar in &self.world.bars {
            if bar.end_ms < view_start || bar.start_ms > self.view_end_ms {
                continue;
            }
            let tr_idx = match bar.kind {
                BarKind::Span => tracks.iter().position(|t| t.tid == Some(bar.thread)),
                BarKind::Heap => tracks.iter().position(|t| t.kind == 1),
                BarKind::Io | BarKind::Lock => tracks.iter().position(|t| t.kind == 2),
            };
            let Some(ti) = tr_idx else {
                continue;
            };
            let tr = &tracks[ti];
            let depth = bar.depth % tr.max_depth;
            let row_key = (ti, depth);

            let raw_x0 = plot_x + (((bar.start_ms - view_start) / self.view_span_ms) as f32) * plot_w;
            let raw_x1 = plot_x + (((bar.end_ms - view_start) / self.view_span_ms) as f32) * plot_w;
            let min_x0 = row_last_x1.get(&row_key).map(|&px1| px1 + 1.5).unwrap_or(plot_x + 1.0);
            let x0 = raw_x0.max(plot_x + 1.0).max(min_x0);
            let x1 = raw_x1.max(x0 + 3.0).min(area.x + area.w);
            let w = x1 - x0;
            if w <= 0.5 || x0 >= area.x + area.w {
                continue;
            }
            row_last_x1.insert(row_key, x1);

            let y = stage_y + tr.y_top - scroll_y + 6.0 + (depth as f32) * (tr.bar_h + 3.0);
            let h = tr.bar_h;
            if y + h < stage_y || y > stage_y + stage_h {
                continue;
            }

            let is_hit = self.mouse_inside
                && self.mouse_x >= plot_x
                && self.mouse_x >= x0 - 2.0
                && self.mouse_x <= x1 + 2.0
                && self.mouse_y >= y.max(stage_y)
                && self.mouse_y <= (y + h).min(stage_y + stage_h);
            if is_hit && best_hover.map(|(bw, _)| w < bw).unwrap_or(true) {
                best_hover = Some((w, *bar));
            }

            let mut base_col: col::Rgba = match bar.kind {
                BarKind::Span => {
                    let hot = bar.dur_ns >= 100_000_000 || (bar.depth == 0 && bar.dur_ns >= 80_000_000);
                    if hot {
                        [0.85, 0.28, 0.11, 0.92]
                    } else if bar.depth > 0 {
                        [0.16, 0.49, 0.82, 0.88]
                    } else {
                        [0.11, 0.56, 0.41, 0.88]
                    }
                }
                BarKind::Heap => [0.22, 0.52, 0.88, 0.88],
                BarKind::Lock => [0.95, 0.38, 0.35, 0.92],
                BarKind::Io => [0.85, 0.58, 0.19, 0.88],
            };
            if is_hit {
                base_col[0] = (base_col[0] * 1.24).min(1.0);
                base_col[1] = (base_col[1] * 1.24).min(1.0);
                base_col[2] = (base_col[2] * 1.24).min(1.0);
                base_col[3] = 1.0;
            }

            self.draw.rrect(x0, y, w, h, 2.5, base_col);
            if w >= 5.0 {
                self.draw.rect(
                    x0 + 1.0,
                    y,
                    w - 2.0,
                    1.0,
                    [
                        (base_col[0] * 1.35).min(1.0),
                        (base_col[1] * 1.35).min(1.0),
                        (base_col[2] * 1.35).min(1.0),
                        0.72,
                    ],
                );
            }

            if bar.kind == BarKind::Lock && bar.holder_thread > 0 {
                if let Some(htr) = tracks.iter().find(|t| t.tid == Some(bar.holder_thread)) {
                    let from_y = stage_y + htr.y_top - scroll_y + htr.height * 0.5;
                    lock_arrows.push((x0 + (w * 0.5).min(8.0), from_y, y, is_hit));
                }
            }

            if w >= 32.0 {
                let lbl = match bar.kind {
                    BarKind::Span => {
                        let name = self.world.fn_name(bar.slot_or_op);
                        if w >= 112.0 {
                            format!("{name} · {}", fmt_ns(bar.dur_ns))
                        } else {
                            name
                        }
                    }
                    BarKind::Heap => fmt_bytes(bar.bytes as i64),
                    BarKind::Lock => {
                        if w >= 112.0 && bar.holder_thread > 0 {
                            format!("mutex (t#{}) · {}", bar.holder_thread, fmt_ns(bar.dur_ns))
                        } else {
                            format!("mutex · {}", fmt_ns(bar.dur_ns))
                        }
                    }
                    BarKind::Io => format!("{} · {}", io_op_name(bar.slot_or_op), fmt_ns(bar.dur_ns)),
                };
                self.draw
                    .text_max(x0 + 5.0, y + (h - 13.0) * 0.5, 10.2, col::WHITE, &lbl, w - 10.0);
            }
        }

        // Lock dependency arrows
        for (ax, from_y, to_y, hi) in lock_arrows {
            let ac = if hi { col::DANGER } else { col::with_alpha(col::DANGER, 0.52) };
            self.draw
                .dashed_line(ax, from_y, ax, to_y - 3.0, if hi { 1.7 } else { 1.1 }, 4.0, 3.0, ac);
            self.draw.circle(ax, from_y, if hi { 3.6 } else { 2.6 }, ac);
            self.draw.line(ax - 3.5, to_y - 6.0, ax, to_y, 1.3, ac);
            self.draw.line(ax + 3.5, to_y - 6.0, ax, to_y, 1.3, ac);
        }

        // Selection box
        if let Some((s0, s1)) = self.selection_ms {
            if s1 >= view_start && s0 <= self.view_end_ms {
                let sx0 = (plot_x + (((s0 - view_start) / self.view_span_ms) as f32) * plot_w).max(plot_x);
                let sx1 = (plot_x + (((s1 - view_start) / self.view_span_ms) as f32) * plot_w).min(area.x + area.w);
                self.draw
                    .rect(sx0, stage_y, (sx1 - sx0).max(1.0), stage_h, col::FIRE_SOFT);
                self.draw.rect(sx0, stage_y, 1.0, stage_h, col::FIRE);
                self.draw.rect(sx1, stage_y, 1.0, stage_h, col::FIRE);
            }
        }

        // Crosshair
        if self.mouse_inside && self.mouse_x >= plot_x && self.mouse_x < area.x + area.w && self.mouse_y >= stage_y && self.mouse_y < stage_y + stage_h {
            self.draw
                .rect(self.mouse_x, stage_y, 1.0, stage_h, [0.92, 0.91, 0.89, 0.32]);
        }

        // Live playhead cursor
        if live_edge >= view_start && live_edge <= self.view_end_ms + PLAYOUT_BUFFER_MS {
            let norm = ((live_edge - view_start) / self.view_span_ms).min(1.0) as f32;
            self.draw
                .rect(plot_x + norm * plot_w - 1.5, stage_y, 1.5, stage_h, col::FIRE);
        }

        self.draw.pop_clip(); // plot clip
        self.draw.pop_clip(); // stage clip

        // Interactive draggable vertical scrollbar for Timeline tracks
        self.draw_vscrollbar(PaneId::Timeline, area.x + area.w - 9.0, stage_y, stage_h, cur_y);

        if let Some((_, bar)) = best_hover {
            self.hovered_bar = Some(bar);
            self.hover_target = Some(HoverTarget::Bar(bar));
        }

        if !self.following {
            let fw = 98.0;
            let fx = area.x + area.w - fw - 10.0;
            let fy = area.y + area.h - 29.0;
            let hov = self.is_hovered(fx, fy, fw, 22.0);
            self.draw.begin_overlay();
            self.draw.card(
                fx,
                fy,
                fw,
                22.0,
                5.0,
                if hov { col::FIRE } else { [0.22, 0.10, 0.07, 0.98] },
                col::FIRE,
            );
            self.draw.text(fx + 9.0, fy + 4.2, 10.5, col::WHITE, "Follow live ↵");
            self.draw.end_overlay();
            self.push_hit(fx, fy, fw, 22.0, Action::FollowLive, "pointer");
        }
    }

    // -----------------------------------------------------------------------
    // Pane: Flame Graph
    // -----------------------------------------------------------------------

    fn render_flame_pane(&mut self, area: Rect) {
        let pad = 10.0;
        let sub_y = area.y + 8.0;

        self.draw
            .card(area.x + pad, sub_y, 152.0, 22.0, 5.0, col::BG2, col::LINE);
        if !self.flame_inverted {
            self.draw
                .rrect(area.x + pad + 1.0, sub_y + 1.0, 74.0, 20.0, 4.0, col::BG4);
        } else {
            self.draw
                .rrect(area.x + pad + 76.0, sub_y + 1.0, 75.0, 20.0, 4.0, col::BG4);
        }
        self.draw.rect(area.x + pad + 75.0, sub_y + 1.0, 1.0, 20.0, col::LINE);
        self.draw.text(
            area.x + pad + 11.0,
            sub_y + 4.5,
            10.5,
            if !self.flame_inverted { col::INK1 } else { col::INK3 },
            "Top-Down",
        );
        self.draw.text(
            area.x + pad + 84.0,
            sub_y + 4.5,
            10.5,
            if self.flame_inverted { col::INK1 } else { col::INK3 },
            "Bottom-Up",
        );
        self.push_hit(area.x + pad, sub_y, 75.0, 22.0, Action::FlameMode(false), "pointer");
        self.push_hit(area.x + pad + 75.0, sub_y, 77.0, 22.0, Action::FlameMode(true), "pointer");

        if let Some(ref z) = self.flame_zoom_name.clone() {
            let rx = area.x + pad + 162.0;
            self.draw_btn(rx, sub_y, 78.0, 22.0, "Reset zoom", false, Action::FlameZoom(None));
            self.draw
                .text(rx + 88.0, sub_y + 4.5, 10.5, col::INK3, &format!("zoom: {z}"));
        }

        let box_y = sub_y + 30.0;
        let box_h = (area.y + area.h - box_y - 10.0).max(40.0);
        let box_w = (area.w - pad * 2.0).max(60.0);
        self.draw
            .card(area.x + pad, box_y, box_w, box_h, 7.0, col::BG2, col::LINE);

        let root = self.world.build_flame_tree(self.selection_ms, self.flame_inverted);
        if root.total_ns == 0 {
            self.draw
                .text(area.x + pad + 14.0, box_y + 14.0, 11.0, col::INK3, "No span call tree yet");
            return;
        }

        let focus = self
            .flame_zoom_name
            .as_deref()
            .and_then(|n| find_flame(&root, n))
            .unwrap_or(&root);
        self.draw
            .push_clip(area.x + pad + 1.0, box_y + 1.0, box_w - 2.0, box_h - 2.0);
        self.draw_flame_node(
            focus,
            0,
            area.x + pad + 6.0,
            box_y + 6.0,
            box_w - 12.0,
            box_y + box_h,
            focus.total_ns.max(1),
            root.total_ns.max(1),
        );
        self.draw.pop_clip();
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_flame_node(
        &mut self,
        node: &FlameNode,
        depth: usize,
        x: f32,
        base_y: f32,
        w: f32,
        max_y: f32,
        root_total: u64,
        global_total: u64,
    ) {
        if w < 2.0 {
            return;
        }
        let row_h = 22.0;
        let y = base_y + (depth as f32) * (row_h + 2.0);
        if y > max_y {
            return;
        }
        let hov = self.is_hovered(x, y, w, row_h);
        let self_share = node.self_ns as f64 / global_total as f64;
        let mut fill = if node.slot < 0 {
            [0.15, 0.15, 0.165, 1.0]
        } else if self_share >= 0.32 {
            [0.85, 0.28, 0.11, 0.94]
        } else if self_share >= 0.16 {
            [0.79, 0.42, 0.15, 0.92]
        } else {
            hash_flame_color(&node.name)
        };
        if hov {
            fill[0] = (fill[0] * 1.22).min(1.0);
            fill[1] = (fill[1] * 1.22).min(1.0);
            fill[2] = (fill[2] * 1.22).min(1.0);
            fill[3] = 1.0;
            self.hover_target = Some(HoverTarget::Flame(
                node.name.clone(),
                node.total_ns,
                node.self_ns,
                node.calls,
                global_total,
            ));
        }

        let fw = (w - 1.5).max(1.0);
        self.draw.rrect(x, y, fw, row_h, 2.5, fill);
        self.draw.rect(
            x + 1.0,
            y,
            (fw - 2.0).max(0.0),
            1.0,
            if hov { [1.0, 1.0, 1.0, 0.5] } else { [1.0, 1.0, 1.0, 0.15] },
        );

        if fw >= 26.0 {
            let pct = (node.total_ns as f64 / root_total as f64) * 100.0;
            let lbl = if fw >= 145.0 {
                format!("{} ({:.1}% · {})", node.name, pct, fmt_ns(node.total_ns))
            } else {
                node.name.clone()
            };
            self.draw.text_max(
                x + 5.0,
                y + 4.5,
                10.2,
                if node.slot < 0 { col::INK2 } else { col::WHITE },
                &lbl,
                fw - 10.0,
            );
        }

        if node.slot < 0 {
            self.push_hit(x, y, fw, row_h, Action::FlameZoom(None), "pointer");
        } else {
            self.push_hit(x, y, fw, row_h, Action::InspectSlot(node.slot as u32), "pointer");
        }

        let mut cx = x;
        let denom = node.total_ns.max(1) as f64;
        for ch in &node.children {
            let cw = ((ch.total_ns as f64 / denom) as f32) * w;
            self.draw_flame_node(ch, depth + 1, cx, base_y, cw, max_y, root_total, global_total);
            cx += cw;
        }
    }

    // -----------------------------------------------------------------------
    // Pane: Code Map (Full-Bleed Semantic Call Graph)
    // -----------------------------------------------------------------------

    fn render_codemap_pane(&mut self, area: Rect) {
        let pad = 8.0;
        let box_x = area.x + pad;
        let box_y = area.y + pad;
        let box_w = (area.w - pad * 2.0).max(80.0);
        let box_h = (area.h - pad * 2.0).max(60.0);
        self.draw
            .card(box_x, box_y, box_w, box_h, 7.0, [0.068, 0.068, 0.074, 1.0], col::LINE);

        self.draw
            .push_clip(box_x + 1.0, box_y + 1.0, box_w - 2.0, box_h - 2.0);

        let step = 26.0 * self.map_scale;
        if step >= 10.0 {
            let ox = ((self.map_pan_x % step) + step) % step;
            let oy = ((self.map_pan_y % step) + step) % step;
            let mut gx = box_x + ox;
            while gx < box_x + box_w {
                let mut gy = box_y + oy;
                while gy < box_y + box_h {
                    self.draw.rect(gx, gy, 1.2, 1.2, [1.0, 1.0, 1.0, 0.055]);
                    gy += step;
                }
                gx += step;
            }
        }

        if self.world.fns.is_empty() {
            self.draw
                .text(box_x + 14.0, box_y + 14.0, 11.0, col::INK3, "Waiting for call graph…");
            self.draw.pop_clip();
            return;
        }

        let mut col0: Vec<(crate::ecs::Entity, FnComp)> = Vec::new();
        let mut col1: Vec<(crate::ecs::Entity, FnComp)> = Vec::new();
        for (e, f) in self.world.fns.iter() {
            if self.world.rels.sources(Rel::Calls, e).is_empty() {
                col0.push((e, f.clone()));
            } else {
                col1.push((e, f.clone()));
            }
        }
        col0.sort_by(|a, b| b.1.total_ns.cmp(&a.1.total_ns));
        col1.sort_by(|a, b| b.1.total_ns.cmp(&a.1.total_ns));
        let cols = if col1.is_empty() { vec![col0] } else { vec![col0, col1] };

        let card_w = (240.0 * self.map_scale).min((box_w * 0.42).max(140.0));
        let card_h = 54.0 * self.map_scale;
        let col_gap = (100.0 * self.map_scale).min((box_w - card_w * 2.0 - 24.0).max(28.0));
        let row_gap = 14.0 * self.map_scale;
        let total_w = cols.len() as f32 * card_w + (cols.len().saturating_sub(1) as f32) * col_gap;
        let start_x = box_x + ((box_w - total_w) * 0.5).max(16.0) + self.map_pan_x;

        let mut placed = Vec::new();
        for (ci, list) in cols.iter().enumerate() {
            let total_h = list.len() as f32 * card_h + (list.len().saturating_sub(1) as f32) * row_gap;
            let start_y = box_y + ((box_h - total_h) * 0.5).max(14.0) + self.map_pan_y;
            for (ri, (e, f)) in list.iter().enumerate() {
                placed.push((
                    *e,
                    f.clone(),
                    start_x + ci as f32 * (card_w + col_gap),
                    start_y + ri as f32 * (card_h + row_gap),
                ));
            }
        }

        let edges: Vec<_> = self.world.rels.iter_rel(Rel::Calls).collect();
        let max_calls = edges.iter().map(|(_, _, e)| e.count).max().unwrap_or(1).max(1);
        for (from_e, to_e, edge) in edges {
            let a = placed.iter().find(|(e, _, _, _)| *e == from_e);
            let b = placed.iter().find(|(e, _, _, _)| *e == to_e);
            let (Some((_, af, ax, ay)), Some((_, bf, bx, by))) = (a, b) else {
                continue;
            };
            let x0 = ax + card_w;
            let y0 = ay + card_h * 0.5;
            let x1 = *bx;
            let y1 = by + card_h * 0.5;
            let mx = (x0 + x1) * 0.5;
            let active = self.inspected_slot == Some(af.slot) || self.inspected_slot == Some(bf.slot);
            let thick = 1.2 + (edge.count as f32 / max_calls as f32) * 2.0;
            let ec = if active { col::FIRE } else { [0.68, 0.67, 0.65, 0.42] };
            self.draw.cubic_bezier(x0, y0, mx, y0, mx, y1, x1, y1, thick, ec);
            self.draw.line(x1 - 7.0, y1 - 4.0, x1, y1, 1.5, ec);
            self.draw.line(x1 - 7.0, y1 + 4.0, x1, y1, 1.5, ec);
            let badge = format!("{}×", edge.count);
            let bw = self.draw.measure_text(&badge, 9.5);
            self.draw.text(mx - bw * 0.5, (y0 + y1) * 0.5 - 12.0, 9.5, col::INK3, &badge);
        }

        let total_self: u64 = self
            .world
            .fns
            .iter()
            .map(|(_, f)| f.total_ns.saturating_sub(f.child_ns))
            .sum::<u64>()
            .max(1);

        // Compute world bounds for the Code Map corner minimap radar
        let mut min_wx = f32::INFINITY;
        let mut min_wy = f32::INFINITY;
        let mut max_wx = f32::NEG_INFINITY;
        let mut max_wy = f32::NEG_INFINITY;
        for (_, _, x, y) in &placed {
            min_wx = min_wx.min(*x);
            min_wy = min_wy.min(*y);
            max_wx = max_wx.max(*x + card_w);
            max_wy = max_wy.max(*y + card_h);
        }
        let placed_radar: Vec<(f32, f32, bool, bool)> = placed
            .iter()
            .map(|(_, f, x, y)| {
                let self_ns = f.total_ns.saturating_sub(f.child_ns);
                let hot = (self_ns as f64 / total_self as f64) >= 0.25;
                (*x, *y, hot, f.lock_contended)
            })
            .collect();

        for (_, f, x, y) in placed {
            let self_ns = f.total_ns.saturating_sub(f.child_ns);
            let share = self_ns as f64 / total_self as f64;
            let hot = share >= 0.25;
            let is_sel = self.inspected_slot == Some(f.slot);
            let is_hov = self.is_hovered(x, y, card_w, card_h);

            let bg = if is_sel {
                [0.135, 0.105, 0.095, 1.0]
            } else if is_hov {
                col::CARD_HOV
            } else {
                col::BG2
            };
            let stroke = if is_sel {
                col::FIRE
            } else if f.lock_contended {
                col::DANGER
            } else if hot {
                col::FIRE_DIM
            } else if is_hov {
                col::LINE2
            } else {
                col::LINE
            };
            self.draw.card(x, y, card_w, card_h, 7.0, bg, stroke);
            self.draw.text_max(
                x + 11.0,
                y + 9.0,
                11.0 * self.map_scale.clamp(0.85, 1.15),
                col::INK1,
                &f.name,
                card_w - if f.rebuilt { 72.0 } else { 20.0 },
            );
            if f.rebuilt && card_w >= 170.0 {
                self.draw.badge(
                    x + card_w - 58.0,
                    y + 8.0,
                    16.0,
                    8.5,
                    "REBUILT",
                    col::SERIES2,
                    col::SERIES2_SOFT,
                    col::with_alpha(col::SERIES2, 0.45),
                );
            }
            let avg = if f.calls > 0 { f.total_ns / f.calls } else { 0 };
            self.draw.text_max(
                x + 11.0,
                y + 30.0 * self.map_scale.clamp(0.85, 1.15),
                10.0 * self.map_scale.clamp(0.85, 1.15),
                if hot { [0.94, 0.54, 0.40, 1.0] } else { col::INK3 },
                &format!("avg {} · {}× · {:.0}% self", fmt_ns(avg), f.calls, share * 100.0),
                card_w - 22.0,
            );
            self.push_hit(x, y, card_w, card_h, Action::InspectSlot(f.slot), "pointer");
        }

        // Interactive Corner Minimap Radar (`108 × 60`) in bottom-right of Code Map
        if !placed_radar.is_empty() && box_w >= 210.0 && box_h >= 120.0 {
            let rw = 108.0;
            let rh = 60.0;
            let rx = box_x + box_w - rw - 8.0;
            let ry = box_y + box_h - rh - 8.0;
            let pad_w = 40.0;
            let wx0 = min_wx.min(box_x) - pad_w;
            let wy0 = min_wy.min(box_y) - pad_w;
            let wx1 = max_wx.max(box_x + box_w) + pad_w;
            let wy1 = max_wy.max(box_y + box_h) + pad_w;
            let ww = (wx1 - wx0).max(1.0);
            let wh = (wy1 - wy0).max(1.0);

            self.draw
                .card(rx, ry, rw, rh, 6.0, [0.052, 0.052, 0.058, 0.92], col::LINE2);

            for (nx, ny, hot, lock) in &placed_radar {
                let mx = rx + 4.0 + ((*nx - wx0) / ww) * (rw - 8.0);
                let my = ry + 4.0 + ((*ny - wy0) / wh) * (rh - 8.0);
                let mw = ((card_w / ww) * (rw - 8.0)).max(6.0);
                let mh = ((card_h / wh) * (rh - 8.0)).max(3.5);
                let c = if *lock {
                    col::DANGER
                } else if *hot {
                    col::FIRE
                } else {
                    col::SERIES3
                };
                self.draw.rrect(mx, my, mw, mh, 1.5, c);
            }

            // Viewport indicator rectangle inside radar
            let vx = rx + 4.0 + ((box_x - wx0) / ww) * (rw - 8.0);
            let vy = ry + 4.0 + ((box_y - wy0) / wh) * (rh - 8.0);
            let vw = ((box_w / ww) * (rw - 8.0)).min(rw - 8.0);
            let vh = ((box_h / wh) * (rh - 8.0)).min(rh - 8.0);
            self.draw
                .border(vx, vy, vw, vh, 2.0, 1.0, col::with_alpha(col::INK1, 0.78));

            self.radar_hit = Some(RadarHit {
                rect: Rect {
                    x: rx,
                    y: ry,
                    w: rw,
                    h: rh,
                },
                world_x: wx0,
                world_y: wy0,
                world_w: ww,
                world_h: wh,
                view_x: box_x,
                view_y: box_y,
                view_w: box_w,
                view_h: box_h,
            });
        }

        self.draw.pop_clip();
    }

    // -----------------------------------------------------------------------
    // Pane: CPU & Topology
    // -----------------------------------------------------------------------

    fn render_cpu_pane(&mut self, area: Rect) {
        let pad = 12.0;
        let top = &self.world.topology;
        let scroll = self.scroll_of(PaneId::CpuTopology);
        let mut y = area.y + 10.0 - scroll;
        let avail_w = (area.w - pad * 2.0).max(240.0);

        let gap = 10.0;
        let card_cols = if avail_w >= 680.0 { 4 } else { 2 };
        let card_w = (avail_w - gap * (card_cols as f32 - 1.0)) / card_cols as f32;
        let card_h = 54.0;

        let cards = [
            ("CPU", top.brand.clone(), format!("{} · {}", top.arch, top.os), col::INK1),
            (
                "TOPOLOGY",
                format!("{} cores ({}P + {}E)", top.ncpu, top.p_cores, top.e_cores),
                format!("{} NUMA · L2 {}", top.numa_nodes, fmt_bytes(top.l2_bytes as i64)),
                col::SERIES2,
            ),
            (
                "MEMORY",
                if top.mem_bytes > 0 { fmt_bytes(top.mem_bytes as i64) } else { "—".into() },
                format!("L1d {} / core", fmt_bytes(top.l1d_bytes as i64)),
                col::OK,
            ),
            (
                "MIGRATIONS",
                format!("{} hops", self.world.total_migrations),
                format!("{} threads", self.world.threads.len()),
                if self.world.total_migrations > 8 { col::FIRE } else { col::INK1 },
            ),
        ];

        for (i, (k, val, sub, accent)) in cards.iter().enumerate() {
            let cx = area.x + pad + (i % card_cols) as f32 * (card_w + gap);
            let cy = y + (i / card_cols) as f32 * (card_h + gap);
            self.draw.card(cx, cy, card_w, card_h, 7.0, col::BG2, col::LINE);
            self.draw.text(cx + 10.0, cy + 7.0, 9.0, col::INK3, k);
            self.draw.text_max(cx + 10.0, cy + 20.0, 11.5, *accent, val, card_w - 20.0);
            self.draw.text_max(cx + 10.0, cy + 37.0, 9.5, col::INK3, sub, card_w - 20.0);
        }
        let card_rows = (4 + card_cols - 1) / card_cols;
        y += card_rows as f32 * (card_h + gap) + 4.0;

        let ncpu = top.ncpu.clamp(1, 32) as usize;
        let max_busy = self.world.cpu_busy_us[..ncpu].iter().copied().max().unwrap_or(1).max(1);
        let cols = if avail_w >= 720.0 { 2 } else { 1 };
        let col_w = (avail_w - gap * (cols as f32 - 1.0)) / cols as f32;
        let row_h = 28.0;
        let rows = (ncpu + cols - 1) / cols;

        let box_h = rows as f32 * row_h + 32.0;
        self.draw
            .card(area.x + pad, y, avail_w, box_h, 7.0, col::BG2, col::LINE);
        self.draw.text(area.x + pad + 12.0, y + 8.0, 10.0, col::INK3, "CORES");

        for c in 0..ncpu {
            let cx = area.x + pad + 12.0 + (c % cols) as f32 * col_w;
            let cy = y + 28.0 + (c / cols) as f32 * row_h;
            let is_e = top.p_cores > 0 && (c as u32) >= top.p_cores;
            let busy_us = self.world.cpu_busy_us[c];
            let frac = (busy_us as f32 / max_busy as f32).clamp(0.0, 1.0);

            self.draw.text(cx, cy + 4.0, 10.5, col::INK1, &format!("Core {:02}", c));
            self.draw.badge(
                cx + 56.0,
                cy + 2.0,
                17.0,
                9.0,
                if is_e { "E" } else { "P" },
                if is_e { col::SERIES2 } else { col::FIRE },
                if is_e { col::SERIES2_SOFT } else { col::FIRE_SOFT },
                if is_e { col::with_alpha(col::SERIES2, 0.4) } else { col::FIRE_DIM },
            );
            let bar_x = cx + 84.0;
            let bar_w = (col_w - 204.0).max(40.0);
            self.draw.rrect(bar_x, cy + 7.0, bar_w, 7.0, 3.5, col::BG4);
            if frac > 0.005 {
                self.draw.rrect(
                    bar_x,
                    cy + 7.0,
                    (bar_w * frac).max(4.0),
                    7.0,
                    3.5,
                    if is_e { col::SERIES2 } else { col::FIRE },
                );
            }
            self.draw.text_max(
                bar_x + bar_w + 10.0,
                cy + 4.0,
                10.0,
                col::INK2,
                &format!("{} · {} ev", fmt_ns(busy_us * 1_000), self.world.cpu_events[c]),
                104.0,
            );
        }

        let content_h = card_rows as f32 * (card_h + gap) + 4.0 + box_h + 24.0;
        self.draw_vscrollbar(PaneId::CpuTopology, area.x + area.w - 9.0, area.y, area.h, content_h);
    }

    // -----------------------------------------------------------------------
    // Pane: Hot Spots
    // -----------------------------------------------------------------------

    fn render_hotspots_pane(&mut self, area: Rect) {
        let pad = 10.0;
        let x0 = area.x + pad;
        let w = (area.w - pad * 2.0).max(280.0);
        let y0 = area.y + 8.0 - self.scroll_of(PaneId::HotSpots);

        let mut list: Vec<FnComp> = self.world.fns.iter().map(|(_, f)| f.clone()).collect();
        list.sort_by(|a, b| b.total_ns.cmp(&a.total_ns));

        let head_h = 24.0;
        let row_h = 28.0;
        let table_h = head_h + (list.len().max(1) as f32) * row_h;
        self.draw.card(x0, y0, w, table_h, 7.0, col::BG2, col::LINE);
        self.draw.rect(x0 + 1.0, y0 + 1.0, w - 2.0, head_h, [1.0, 1.0, 1.0, 0.03]);
        self.draw.rect(x0, y0 + head_h, w, 1.0, col::LINE);

        let cols_x = [
            x0 + 10.0,
            x0 + w * 0.29,
            x0 + w * 0.36,
            x0 + w * 0.45,
            x0 + w * 0.54,
            x0 + w * 0.65,
            x0 + w * 0.72,
            x0 + w * 0.79,
            x0 + w * 0.86,
        ];
        for (i, h) in ["SYMBOL", "CALLS", "SELF", "TOTAL", "SHARE", "P50", "P95", "P99", "HOOK"].iter().enumerate() {
            self.draw.text(cols_x[i], y0 + 6.0, 9.5, col::INK3, h);
        }

        if list.is_empty() {
            self.draw.text(x0 + 10.0, y0 + head_h + 8.0, 10.5, col::INK3, "—");
            self.pane_max_scroll.insert(PaneId::HotSpots, 0.0);
            return;
        }

        let total_all: u64 = list.iter().map(|f| f.total_ns).sum::<u64>().max(1);
        for (ri, f) in list.iter().enumerate() {
            let ry = y0 + head_h + ri as f32 * row_h;
            if ri > 0 {
                self.draw.rect(x0, ry, w, 1.0, col::LINE);
            }
            let is_sel = self.inspected_slot == Some(f.slot);
            if is_sel {
                self.draw.rect(x0 + 1.0, ry + 1.0, w - 2.0, row_h - 1.0, col::FIRE_SOFT);
            } else if self.is_hovered(x0, ry, w, row_h) {
                self.draw.rect(x0 + 1.0, ry + 1.0, w - 2.0, row_h - 1.0, [1.0, 1.0, 1.0, 0.035]);
            }

            let self_ns = f.total_ns.saturating_sub(f.child_ns);
            let pct = (f.total_ns as f64 / total_all as f64) as f32;
            let ty = ry + 7.5;
            self.draw
                .text_max(cols_x[0], ty, 10.5, col::INK1, &f.name, cols_x[1] - cols_x[0] - 8.0);
            self.draw.text(cols_x[1], ty, 10.5, col::INK2, &f.calls.to_string());
            self.draw.text(cols_x[2], ty, 10.5, col::INK1, &fmt_ns(self_ns));
            self.draw.text(cols_x[3], ty, 10.5, col::INK2, &fmt_ns(f.total_ns));

            let bw = (cols_x[5] - cols_x[4] - 12.0).max(24.0);
            self.draw.rrect(cols_x[4], ry + 12.0, bw, 4.5, 2.0, col::BG4);
            self.draw.rrect(
                cols_x[4],
                ry + 12.0,
                (bw * pct).clamp(3.0, bw),
                4.5,
                2.0,
                if pct >= 0.25 { col::FIRE } else { col::SERIES2 },
            );

            self.draw.text(cols_x[5], ty, 10.5, col::INK2, &fmt_ns(percentile(&f.samples, 0.5)));
            self.draw.text(cols_x[6], ty, 10.5, col::INK2, &fmt_ns(percentile(&f.samples, 0.95)));
            self.draw.text(cols_x[7], ty, 10.5, col::INK2, &fmt_ns(percentile(&f.samples, 0.99)));
            self.push_hit(x0, ry, cols_x[8] - x0 - 4.0, row_h, Action::InspectSlot(f.slot), "pointer");

            let hx = cols_x[8];
            let seg_bw = ((x0 + w - hx - 8.0) / 4.0).clamp(16.0, 21.0);
            self.draw.card(hx, ry + 5.0, seg_bw * 4.0, 18.0, 4.0, col::BG, col::LINE);
            for (hi, hl) in ["P", "C", "H", "S"].iter().enumerate() {
                let bx = hx + hi as f32 * seg_bw;
                let active = f.hook == hi as u8;
                if active {
                    self.draw.rrect(bx + 1.0, ry + 6.0, seg_bw - 2.0, 16.0, 3.0, col::BG4);
                }
                self.draw
                    .text(bx + (seg_bw - 6.0) * 0.5, ry + 7.5, 9.5, if active { col::INK1 } else { col::INK3 }, hl);
                self.push_hit(bx, ry + 5.0, seg_bw, 18.0, Action::SetHook(f.slot, hi as u8), "pointer");
            }
        }
        self.draw_vscrollbar(PaneId::HotSpots, area.x + area.w - 9.0, area.y, area.h, table_h + 24.0);
    }

    // -----------------------------------------------------------------------
    // Pane: Heap & Diff
    // -----------------------------------------------------------------------

    fn render_heap_pane(&mut self, area: Rect) {
        let pad = 10.0;
        let sub_y = area.y + 8.0;
        self.draw_btn(area.x + pad, sub_y, 108.0, 22.0, "Take Snapshot", false, Action::HeapTakeSnap);
        let mut lx = area.x + pad + 116.0;
        if self.world.heap_baseline.is_some() {
            self.draw_btn(lx, sub_y, 102.0, 22.0, "Clear Baseline", false, Action::HeapClearSnap);
            lx += 110.0;
        }
        let status = if self.world.heap_baseline.is_some() {
            format!("Diff vs snapshot ({})", self.world.heap_baseline_label)
        } else {
            format!(
                "{} retained · {} live",
                fmt_bytes(self.world.total_retained_bytes() as i64),
                self.world.live_ptrs.len()
            )
        };
        self.draw.text(lx, sub_y + 4.5, 10.5, col::INK3, &status);

        let x0 = area.x + pad;
        let w = (area.w - pad * 2.0).max(280.0);
        let y0 = sub_y + 30.0 - self.scroll_of(PaneId::HeapDiff);

        let mut sites: Vec<_> = self.world.heap_sites.iter().map(|(_, s)| s.clone()).collect();
        sites.sort_by(|a, b| b.retained_bytes.cmp(&a.retained_bytes));

        let head_h = 24.0;
        let row_h = 26.0;
        let table_h = head_h + (sites.len().max(1) as f32) * row_h;
        self.draw.card(x0, y0, w, table_h, 7.0, col::BG2, col::LINE);
        self.draw.rect(x0 + 1.0, y0 + 1.0, w - 2.0, head_h, [1.0, 1.0, 1.0, 0.03]);
        self.draw.rect(x0, y0 + head_h, w, 1.0, col::LINE);

        let cx = [x0 + 10.0, x0 + w * 0.42, x0 + w * 0.50, x0 + w * 0.58, x0 + w * 0.70, x0 + w * 0.82];
        for (i, h) in ["CALLER STACK", "ALLOCS", "FREES", "CUMULATIVE", "RETAINED", "Δ SNAP"].iter().enumerate() {
            self.draw.text(cx[i], y0 + 6.0, 9.5, col::INK3, h);
        }

        if sites.is_empty() {
            self.draw.text(x0 + 10.0, y0 + head_h + 7.0, 10.5, col::INK3, "—");
            self.pane_max_scroll.insert(PaneId::HeapDiff, 0.0);
            return;
        }
        for (ri, s) in sites.iter().enumerate() {
            let ry = y0 + head_h + ri as f32 * row_h;
            if ri > 0 {
                self.draw.rect(x0, ry, w, 1.0, col::LINE);
            }
            let ty = ry + 6.5;
            self.draw.text_max(cx[0], ty, 10.5, col::INK1, &s.key, cx[1] - cx[0] - 8.0);
            self.draw.text(cx[1], ty, 10.5, col::INK2, &s.allocs.to_string());
            self.draw.text(cx[2], ty, 10.5, col::INK2, &s.frees.to_string());
            self.draw.text(cx[3], ty, 10.5, col::INK2, &fmt_bytes(s.total_bytes as i64));
            self.draw.text(cx[4], ty, 10.5, col::INK1, &fmt_bytes(s.retained_bytes as i64));
            if let Some(ref snap) = self.world.heap_baseline {
                let prev = snap.get(&s.key).copied().unwrap_or(0) as i64;
                let diff = s.retained_bytes as i64 - prev;
                self.draw.text(
                    cx[5],
                    ty,
                    10.5,
                    if diff > 0 { col::FIRE } else if diff < 0 { col::OK } else { col::INK3 },
                    &format!("{}{}", if diff >= 0 { "+" } else { "" }, fmt_bytes(diff)),
                );
            } else {
                self.draw.text(cx[5], ty, 10.5, col::INK4, "—");
            }
        }
        self.draw_vscrollbar(PaneId::HeapDiff, area.x + area.w - 9.0, area.y, area.h, table_h + 52.0);
    }

    // -----------------------------------------------------------------------
    // Pane: Builds
    // -----------------------------------------------------------------------

    fn render_builds_pane(&mut self, area: Rect) {
        let pad = 10.0;
        let avail_w = (area.w - pad * 2.0).max(240.0);
        let y0 = area.y + 10.0 - self.scroll_of(PaneId::Builds);

        if self.world.builds.is_empty() {
            self.draw.text(area.x + pad, y0 + 6.0, 11.0, col::INK3, "No builds recorded");
            self.pane_max_scroll.insert(PaneId::Builds, 0.0);
            return;
        }

        let cols = if avail_w >= 720.0 { 2 } else { 1 };
        let gap = 10.0;
        let card_w = (avail_w - gap * (cols as f32 - 1.0)) / cols as f32;
        let card_h = 112.0;
        let series = [col::SERIES1, col::SERIES2, col::SERIES3, col::SERIES4];

        for (i, b) in self.world.builds.iter().enumerate() {
            let cx = area.x + pad + (i % cols) as f32 * (card_w + gap);
            let cy = y0 + (i / cols) as f32 * (card_h + gap);
            self.draw.card(cx, cy, card_w, card_h, 7.0, col::BG2, col::LINE);

            let bw = self
                .draw
                .badge(cx + 10.0, cy + 10.0, 18.0, 9.0, &b.command, col::INK2, col::BG3, col::LINE);
            let pw = self.draw.text(cx + 16.0 + bw, cy + 12.0, 11.5, col::INK1, &b.package);
            let (st_lbl, st_fg, st_bg) = match b.status.as_str() {
                "failed" => ("FAILED", col::DANGER, col::DANGER_SOFT),
                "running" | "building" => ("LIVE", col::FIRE, col::FIRE_SOFT),
                _ => ("OK", col::OK, col::OK_SOFT),
            };
            self.draw.badge(
                cx + 22.0 + bw + pw,
                cy + 10.0,
                18.0,
                9.0,
                st_lbl,
                st_fg,
                st_bg,
                col::with_alpha(st_fg, 0.4),
            );

            let total_ns: u64 = b.phases.iter().map(|p| p.ns).sum();
            let meta = format!("{} built · {}/{} · {}", b.made, b.found, b.reached, fmt_ns(total_ns));
            let mw = self.draw.measure_text(&meta, 10.0);
            self.draw.text(cx + card_w - mw - 10.0, cy + 12.5, 10.0, col::INK3, &meta);

            let hit_pct = if b.reached > 0 {
                (b.found as f32 / b.reached as f32).clamp(0.0, 1.0)
            } else {
                1.0
            };
            self.draw
                .text_max(cx + 10.0, cy + 36.0, 10.5, col::INK2, &b.recompile_reason, card_w - 130.0);
            self.draw.rrect(cx + card_w - 108.0, cy + 41.0, 66.0, 4.5, 2.0, col::BG4);
            self.draw.rrect(
                cx + card_w - 108.0,
                cy + 41.0,
                (66.0 * hit_pct).max(3.0),
                4.5,
                2.0,
                if hit_pct >= 0.7 { col::OK } else { col::FIRE },
            );
            self.draw.text(
                cx + card_w - 36.0,
                cy + 36.5,
                10.0,
                col::INK3,
                &format!("{}%", (hit_pct * 100.0).round() as u32),
            );

            if total_ns > 0 {
                let mut wx = cx + 10.0;
                let wf_w = card_w - 20.0;
                let wy = cy + 58.0;
                for (pi, ph) in b.phases.iter().enumerate() {
                    let frac = (ph.ns as f32 / total_ns as f32).max(0.03);
                    let sw = (wf_w * frac - 2.0).max(4.0);
                    self.draw.rrect(wx, wy, sw, 18.0, 3.0, series[pi % series.len()]);
                    if sw >= 42.0 {
                        self.draw.text_max(wx + 5.0, wy + 3.0, 9.5, col::WHITE, &ph.name, sw - 8.0);
                    }
                    wx += sw + 2.0;
                }
                let mut lx = cx + 10.0;
                let ly = cy + 84.0;
                for (pi, ph) in b.phases.iter().enumerate() {
                    let item = format!("{} {}", ph.name, fmt_ns(ph.ns));
                    let iw = self.draw.measure_text(&item, 9.5) + 18.0;
                    if lx + iw > cx + card_w - 6.0 {
                        break;
                    }
                    self.draw.rrect(lx, ly + 3.0, 6.0, 6.0, 1.5, series[pi % series.len()]);
                    self.draw.text(lx + 9.0, ly, 9.5, col::INK3, &item);
                    lx += iw;
                }
            }
        }
        let rows = (self.world.builds.len() + cols - 1) / cols;
        self.draw_vscrollbar(
            PaneId::Builds,
            area.x + area.w - 9.0,
            area.y,
            area.h,
            rows as f32 * (card_h + gap) + 20.0,
        );
    }

    // -----------------------------------------------------------------------
    // Pane: Advisor
    // -----------------------------------------------------------------------

    fn render_advisor_pane(&mut self, area: Rect, insights: &[Insight]) {
        let pad = 10.0;
        let x0 = area.x + pad;
        let w = (area.w - pad * 2.0).max(240.0);
        let y0 = area.y + 10.0 - self.scroll_of(PaneId::Advisor);

        if insights.is_empty() {
            self.draw.text(x0, y0 + 6.0, 11.0, col::INK3, "0 issues");
            self.pane_max_scroll.insert(PaneId::Advisor, 0.0);
            return;
        }

        let row_h = 34.0;
        let box_h = insights.len() as f32 * row_h;
        self.draw.card(x0, y0, w, box_h, 7.0, col::BG2, col::LINE);

        for (i, ins) in insights.iter().enumerate() {
            let ry = y0 + i as f32 * row_h;
            if i > 0 {
                self.draw.rect(x0, ry, w, 1.0, col::LINE);
            }
            let (fg, bg) = match ins.severity {
                2 => (col::DANGER, col::DANGER_SOFT),
                1 => (col::FIRE, col::FIRE_SOFT),
                _ => (col::OK, col::OK_SOFT),
            };
            let ico = match ins.category {
                "HOT" => Icon::Flame,
                "LOCK" => Icon::Lock,
                "HEAP" => Icon::Memory,
                "CPU" => Icon::Cpu,
                _ => Icon::Layers,
            };
            // Symbol-first circular/rounded icon chip instead of text tag!
            self.draw
                .card(x0 + 10.0, ry + 6.0, 22.0, 22.0, 5.0, bg, col::with_alpha(fg, 0.45));
            self.draw.icon(ico, x0 + 14.5, ry + 10.5, 13.0, fg);

            let tx = x0 + 40.0;
            let tw = self.draw.text(tx, ry + 10.0, 11.0, col::INK1, &ins.title);
            self.draw.text_max(
                tx + tw + 10.0,
                ry + 10.0,
                10.0,
                col::INK3,
                &ins.detail,
                (w - (tx + tw - x0) - 140.0).max(16.0),
            );
            let mw = self.draw.measure_text(&ins.metric, 10.0) + 12.0;
            self.draw
                .badge(x0 + w - mw - 38.0, ry + 8.0, 18.0, 10.0, &ins.metric, fg, bg, col::with_alpha(fg, 0.4));
            let target_pane = match ins.target_tab {
                2 => PaneId::CpuTopology,
                3 => PaneId::HotSpots,
                4 => PaneId::HeapDiff,
                5 => PaneId::Builds,
                _ => PaneId::IoLocks,
            };
            self.draw_btn(x0 + w - 32.0, ry + 6.0, 24.0, 22.0, "→", false, Action::OpenPane(target_pane));
        }
        self.draw_vscrollbar(PaneId::Advisor, area.x + area.w - 9.0, area.y, area.h, box_h + 24.0);
    }

    // -----------------------------------------------------------------------
    // Pane: I/O & Locks
    // -----------------------------------------------------------------------

    fn render_io_pane(&mut self, area: Rect) {
        let pad = 10.0;
        let x0 = area.x + pad;
        let w = (area.w - pad * 2.0).max(280.0);
        let y0 = area.y + 8.0 - self.scroll_of(PaneId::IoLocks);

        let rows: Vec<_> = self.world.io_rows.iter().rev().take(80).collect();
        let head_h = 24.0;
        let row_h = 26.0;
        let table_h = head_h + (rows.len().max(1) as f32) * row_h;
        self.draw.card(x0, y0, w, table_h, 7.0, col::BG2, col::LINE);
        self.draw.rect(x0 + 1.0, y0 + 1.0, w - 2.0, head_h, [1.0, 1.0, 1.0, 0.03]);
        self.draw.rect(x0, y0 + head_h, w, 1.0, col::LINE);

        let cx = [x0 + 10.0, x0 + w * 0.14, x0 + w * 0.24, x0 + w * 0.34, x0 + w * 0.48, x0 + w * 0.62];
        for (i, h) in ["KIND", "THREAD", "CORE", "OP", "DURATION", "ATTRIBUTION"].iter().enumerate() {
            self.draw.text(cx[i], y0 + 6.0, 9.5, col::INK3, h);
        }

        if rows.is_empty() {
            self.draw.text(x0 + 10.0, y0 + head_h + 7.0, 10.5, col::INK3, "—");
            self.pane_max_scroll.insert(PaneId::IoLocks, 0.0);
            return;
        }
        for (ri, r) in rows.iter().enumerate() {
            let ry = y0 + head_h + ri as f32 * row_h;
            if ri > 0 {
                self.draw.rect(x0, ry, w, 1.0, col::LINE);
            }
            let ty = ry + 6.5;
            let is_lock = r.kind == "sync_wait";
            self.draw
                .text(cx[0], ty, 10.5, if is_lock { col::DANGER } else { col::WARN }, r.kind);
            self.draw.text(cx[1], ty, 10.5, col::INK2, &format!("t#{}", r.thread));
            let core_s = if r.cpu != CPU_UNKNOWN {
                format!("C{} · {}", r.cpu, if (r.flags & 0x10) != 0 { "E" } else { "P" })
            } else {
                "—".into()
            };
            self.draw.text(cx[2], ty, 10.5, col::INK3, &core_s);
            self.draw.text(cx[3], ty, 10.5, col::INK1, io_op_name(r.op));
            self.draw.text(cx[4], ty, 10.5, col::INK1, &fmt_ns(r.dur_ns));
            let attr = if is_lock && r.holder_thread > 0 {
                format!("holder t#{} @ 0x{:x} · mutex 0x{:x}", r.holder_thread, r.holder_pc, r.arg)
            } else {
                format!("fd={} · bytes={}", r.arg, r.res)
            };
            self.draw.text_max(cx[5], ty, 10.0, col::INK2, &attr, x0 + w - cx[5] - 10.0);
        }
        self.draw_vscrollbar(PaneId::IoLocks, area.x + area.w - 9.0, area.y, area.h, table_h + 24.0);
    }

    // -----------------------------------------------------------------------
    // Pane: Modular Read-Only Post-IDE Inspector (Dockable Anywhere + Expandable!)
    // -----------------------------------------------------------------------

    fn render_inspector_pane(&mut self, area: Rect) {
        self.draw
            .rect(area.x, area.y, area.w, area.h, [0.071, 0.071, 0.076, 1.0]);

        let Some(slot) = self.inspected_slot.or_else(|| {
            self.world
                .fns
                .iter()
                .max_by_key(|(_, f)| f.total_ns)
                .map(|(_, f)| f.slot)
        }) else {
            self.draw.text(
                area.x + 14.0,
                area.y + 14.0,
                11.0,
                col::INK3,
                "Click any span, flame frame, or Code Map node",
            );
            return;
        };

        let Some(&fe) = self.world.slot_entity.get(&slot) else {
            return;
        };
        let Some(f) = self.world.fns.get(fe).cloned() else {
            return;
        };

        if !f.file.is_empty() && f.line > 0 {
            self.queue_source_if_needed(&f.file, f.line);
        }

        let pad = 14.0;
        let scroll = self.scroll_of(PaneId::Inspector);
        let mut y = area.y + 12.0 - scroll;

        self.draw
            .text_max(area.x + pad, y, 12.5, col::INK1, &f.name, area.w - 86.0);
        if !f.file.is_empty() && f.line > 0 {
            self.draw_btn(
                area.x + area.w - 66.0,
                y - 3.0,
                52.0,
                22.0,
                "Open ↗",
                false,
                Action::OpenEditor(f.file.clone(), f.line),
            );
        }
        y += 18.0;

        let loc_str = if !f.file.is_empty() {
            format!("{}:{}", short_path(&f.file), f.line)
        } else {
            format!("slot #{slot}")
        };
        self.draw
            .text_max(area.x + pad, y, 10.0, col::INK3, &loc_str, area.w - pad * 2.0);
        y += 18.0;

        let mut bx = area.x + pad;
        if f.last_cpu != CPU_UNKNOWN {
            let is_e = (f.last_flags & 0x10) != 0;
            bx += self.draw.badge(
                bx,
                y,
                18.0,
                9.5,
                &format!("Core #{} ({})", f.last_cpu, if is_e { "E-core" } else { "P-core" }),
                if is_e { col::SERIES2 } else { col::INK2 },
                col::BG3,
                col::LINE,
            ) + 6.0;
        }
        if f.rebuilt {
            bx += self.draw.badge(
                bx,
                y,
                18.0,
                9.5,
                "REBUILT",
                col::SERIES2,
                col::SERIES2_SOFT,
                col::with_alpha(col::SERIES2, 0.45),
            ) + 6.0;
        }
        if f.lock_contended {
            self.draw.badge(
                bx,
                y,
                18.0,
                9.5,
                "LOCK WAIT",
                col::DANGER,
                col::DANGER_SOFT,
                col::with_alpha(col::DANGER, 0.45),
            );
        }
        y += 26.0;
        self.draw.rect(area.x, y, area.w, 1.0, col::LINE);
        y += 12.0;

        let self_ns = f.total_ns.saturating_sub(f.child_ns);
        let avg_ns = if f.calls > 0 { f.total_ns / f.calls } else { 0 };
        let stats = [
            ("SELF", fmt_ns(self_ns), col::FIRE),
            ("TOTAL", fmt_ns(f.total_ns), col::INK1),
            ("AVG", fmt_ns(avg_ns), col::INK1),
            ("P95", fmt_ns(percentile(&f.samples, 0.95)), col::WARN),
            ("P99", fmt_ns(percentile(&f.samples, 0.99)), col::INK2),
            ("CALLS", format!("{}×", f.calls), col::OK),
        ];
        let sg = 7.0;
        let sw = (area.w - pad * 2.0 - sg * 2.0) / 3.0;
        let sh = 42.0;
        for (i, (k, v, c)) in stats.iter().enumerate() {
            let sx = area.x + pad + (i % 3) as f32 * (sw + sg);
            let sy = y + (i / 3) as f32 * (sh + sg);
            self.draw.card(sx, sy, sw, sh, 6.0, col::BG2, col::LINE);
            self.draw.text(sx + 8.0, sy + 6.0, 9.0, col::INK3, k);
            self.draw.text_max(sx + 8.0, sy + 20.0, 11.5, *c, v, sw - 14.0);
        }
        y += sh * 2.0 + sg + 14.0;

        self.draw.text(area.x + pad, y, 9.5, col::INK3, "HOOK");
        y += 15.0;
        let seg_w = (area.w - pad * 2.0).max(120.0);
        let btn_w = seg_w / 4.0;
        self.draw.card(area.x + pad, y, seg_w, 24.0, 5.0, col::BG2, col::LINE);
        for (hi, name) in ["Plain", "Count", "Hook", "Span"].iter().enumerate() {
            let hx = area.x + pad + hi as f32 * btn_w;
            let active = f.hook == hi as u8;
            if active {
                self.draw
                    .rrect(hx + 1.0, y + 1.0, btn_w - 2.0, 22.0, 4.0, col::FIRE_SOFT);
            }
            if hi > 0 {
                self.draw.rect(hx, y + 1.0, 1.0, 22.0, col::LINE);
            }
            let tw = self.draw.measure_text(name, 10.5);
            self.draw.text(
                hx + (btn_w - tw) * 0.5,
                y + 5.5,
                10.5,
                if active { col::FIRE } else { col::INK2 },
                name,
            );
            self.push_hit(hx, y, btn_w, 24.0, Action::SetHook(slot, hi as u8), "pointer");
        }
        y += 34.0;

        let callers: Vec<(u32, String, u64)> = self
            .world
            .rels
            .sources(Rel::Calls, fe)
            .iter()
            .filter_map(|(ce, edge)| self.world.fns.get(*ce).map(|cf| (cf.slot, cf.name.clone(), edge.count)))
            .collect();
        let callees: Vec<(u32, String, u64)> = self
            .world
            .rels
            .targets(fe, Rel::Calls)
            .iter()
            .filter_map(|(ce, edge)| self.world.fns.get(*ce).map(|cf| (cf.slot, cf.name.clone(), edge.count)))
            .collect();

        if !callers.is_empty() {
            self.draw.text(area.x + pad, y, 9.5, col::INK3, "CALLERS");
            y += 15.0;
            let mut cx = area.x + pad;
            for (cslot, cname, cnt) in callers.iter().take(4) {
                let lbl = format!("{cname} · {cnt}×");
                let cw = (self.draw.measure_text(&lbl, 10.0) + 14.0).min(area.w - pad * 2.0);
                if cx + cw > area.x + area.w - pad {
                    cx = area.x + pad;
                    y += 24.0;
                }
                self.draw_btn(cx, y, cw, 20.0, &lbl, false, Action::InspectSlot(*cslot));
                cx += cw + 6.0;
            }
            y += 28.0;
        }
        if !callees.is_empty() {
            self.draw.text(area.x + pad, y, 9.5, col::INK3, "CALLEES");
            y += 15.0;
            let mut cx = area.x + pad;
            for (cslot, cname, cnt) in callees.iter().take(4) {
                let lbl = format!("{cname} · {cnt}×");
                let cw = (self.draw.measure_text(&lbl, 10.0) + 14.0).min(area.w - pad * 2.0);
                if cx + cw > area.x + area.w - pad {
                    cx = area.x + pad;
                    y += 24.0;
                }
                self.draw_btn(cx, y, cw, 20.0, &lbl, false, Action::InspectSlot(*cslot));
                cx += cw + 6.0;
            }
            y += 28.0;
        }

        self.draw.text(area.x + pad, y, 9.5, col::INK3, "SOURCE");
        y += 15.0;
        let line_h = 16.5;
        let num_lines = self
            .world
            .sources
            .get(&(f.file.clone(), f.line))
            .map(|v| v.len())
            .unwrap_or(6);
        let unscrolled_y = (y + scroll) - area.y;
        let box_h = ((num_lines as f32 * line_h) + 14.0).max((area.h - unscrolled_y - 12.0).max(90.0));
        let box_w = (area.w - pad * 2.0 - 6.0).max(80.0);
        let code_gutter = 38.0;
        let code_view_w = (box_w - code_gutter - 8.0).max(40.0);
        let max_line_w = self
            .world
            .sources
            .get(&(f.file.clone(), f.line))
            .map(|lines| {
                lines
                    .iter()
                    .map(|(_, t)| self.draw.measure_text(t, 10.2) + 16.0)
                    .fold(0.0_f32, f32::max)
            })
            .unwrap_or(0.0);
        self.source_max_scroll_x = (max_line_w - code_view_w).max(0.0);
        self.source_scroll_x = self.source_scroll_x.clamp(0.0, self.source_max_scroll_x);
        let has_hscroll = self.source_max_scroll_x > 2.0;
        let box_h = box_h + if has_hscroll { 12.0 } else { 0.0 };

        self.draw
            .card(area.x + pad, y, box_w, box_h, 7.0, [0.055, 0.055, 0.060, 1.0], col::LINE);

        self.draw
            .push_clip(area.x + pad + 1.0, y + 1.0, box_w - 2.0, box_h - 2.0);
        if let Some(lines) = self.world.sources.get(&(f.file.clone(), f.line)) {
            let mut ly = y + 7.0;
            for (n, text) in lines {
                let is_t = *n == f.line;
                if is_t {
                    self.draw
                        .rect(area.x + pad + 1.0, ly - 1.0, box_w - 2.0, line_h, col::FIRE_SOFT);
                }
                // Sticky line numbers
                self.draw.text(
                    area.x + pad + 8.0,
                    ly + 1.5,
                    9.8,
                    if is_t { col::FIRE } else { col::INK4 },
                    &format!("{n:>4}"),
                );
                // Horizontally scrollable syntax-highlighted Rust code (clipped cleanly after line numbers!)
                self.draw.push_clip(
                    area.x + pad + code_gutter,
                    ly - 1.0,
                    code_view_w,
                    line_h + 2.0,
                );
                self.draw.rust_code_line(
                    area.x + pad + code_gutter + 2.0 - self.source_scroll_x,
                    ly + 1.5,
                    10.2,
                    max_line_w + 40.0,
                    text,
                );
                self.draw.pop_clip();
                ly += line_h;
            }
        } else {
            self.draw.text(
                area.x + pad + 12.0,
                y + 12.0,
                10.2,
                col::INK3,
                if f.file.is_empty() { "No source path" } else { "Loading…" },
            );
        }

        // Draggable horizontal scrollbar inside SOURCE box when lines overflow horizontally
        if has_hscroll {
            let track = Rect {
                x: area.x + pad + code_gutter,
                y: y + box_h - 10.0,
                w: (code_view_w - 4.0).max(24.0),
                h: 7.0,
            };
            let thumb_w = ((code_view_w / max_line_w.max(1.0)) * track.w).clamp(24.0, track.w);
            let thumb_x = track.x + (self.source_scroll_x / self.source_max_scroll_x) * (track.w - thumb_w);
            let thumb = Rect {
                x: thumb_x,
                y: track.y + 1.0,
                w: thumb_w,
                h: 5.0,
            };
            let dragging = matches!(self.drag_op, DragOp::HScrollSource { .. });
            let hov = self.is_hovered(track.x, track.y - 2.0, track.w, track.h + 4.0);
            self.draw
                .rrect(track.x, track.y + 0.5, track.w, 6.0, 3.0, [0.09, 0.09, 0.10, 0.9]);
            self.draw.rrect(
                thumb.x,
                thumb.y,
                thumb.w,
                thumb.h,
                2.5,
                if dragging {
                    col::FIRE
                } else if hov {
                    col::INK2
                } else {
                    [0.36, 0.36, 0.39, 0.85]
                },
            );
            self.hscroll_source = Some(HScrollHit {
                track,
                thumb,
                max_scroll: self.source_max_scroll_x,
            });
        }
        self.draw.pop_clip();

        let total_h = (y + scroll - area.y) + box_h + 16.0;
        self.draw_vscrollbar(PaneId::Inspector, area.x + area.w - 9.0, area.y, area.h, total_h);
    }

    // -----------------------------------------------------------------------
    // Z-Pass 2 Overlays: 5-Zone Drag Docking Preview, Panes Popover, 2-Zone Hover Card
    // -----------------------------------------------------------------------

    fn render_dock_overlay(&mut self, pane: PaneId, leaves: &[LeafLayout]) {
        self.draw.begin_overlay();
        if let Some(target) = leaves.iter().find(|lf| lf.rect.contains(self.mouse_x, self.mouse_y)) {
            let zone = compute_drop_zone(target.rect, self.mouse_x, self.mouse_y);
            let pr = drop_preview_rect(target.rect, zone);
            self.draw
                .card(pr.x + 4.0, pr.y + 4.0, pr.w - 8.0, pr.h - 8.0, 8.0, col::FIRE_SOFT, col::FIRE);
            let zlbl = match zone {
                DropZone::Left => "Dock Left",
                DropZone::Right => "Dock Right",
                DropZone::Top => "Dock Top",
                DropZone::Bottom => "Dock Bottom",
                DropZone::Center => "Dock as Tab",
            };
            let zw = self.draw.measure_text(zlbl, 11.0);
            self.draw.badge(
                pr.x + (pr.w - zw - 12.0) * 0.5,
                pr.y + (pr.h - 22.0) * 0.5,
                22.0,
                11.0,
                zlbl,
                col::WHITE,
                [0.14, 0.08, 0.06, 0.96],
                col::FIRE,
            );
        }

        // Floating dragged tab ghost following pointer
        let title = pane.title();
        let gw = self.draw.measure_text(title, 10.5) + 20.0;
        self.draw.card(
            self.mouse_x - gw * 0.5,
            self.mouse_y - 12.0,
            gw,
            24.0,
            6.0,
            [0.12, 0.12, 0.13, 0.96],
            col::FIRE,
        );
        self.draw
            .text(self.mouse_x - gw * 0.5 + 10.0, self.mouse_y - 6.0, 10.5, col::INK1, title);
        self.draw.end_overlay();
    }

    fn render_layout_menu(&mut self, css_w: f32) {
        self.draw.begin_overlay();
        let mw = 190.0;
        let row_h = 24.0;
        let mh = (PaneId::ALL.len() as f32 + 1.0) * row_h + 18.0;
        let mx = (css_w - mw - 12.0).max(8.0);
        let my = TOPBAR_H + 4.0;

        self.draw
            .rrect(mx + 3.0, my + 5.0, mw, mh, 8.0, [0.0, 0.0, 0.0, 0.55]);
        self.draw
            .card(mx, my, mw, mh, 8.0, [0.078, 0.078, 0.084, 0.99], col::LINE2);

        let mut y = my + 6.0;
        for &pane in &PaneId::ALL {
            let open = self.dock.contains_pane(pane);
            let hov = self.is_hovered(mx + 4.0, y, mw - 8.0, row_h);
            if hov {
                self.draw.rrect(mx + 4.0, y, mw - 8.0, row_h, 4.0, col::BG3);
            }
            self.draw.icon(
                pane_icon(pane),
                mx + 10.0,
                y + 6.0,
                12.0,
                if open { col::FIRE } else { col::INK4 },
            );
            self.draw
                .text(mx + 28.0, y + 5.5, 10.5, if open { col::INK1 } else { col::INK3 }, pane.title());
            if open {
                self.draw.circle(mx + mw - 14.0, y + row_h * 0.5, 2.5, col::OK);
            }
            self.push_hit(mx + 4.0, y, mw - 8.0, row_h, Action::OpenPane(pane), "pointer");
            y += row_h;
        }
        self.draw.rect(mx + 6.0, y + 3.0, mw - 12.0, 1.0, col::LINE);
        y += 7.0;
        let hov = self.is_hovered(mx + 4.0, y, mw - 8.0, row_h);
        if hov {
            self.draw.rrect(mx + 4.0, y, mw - 8.0, row_h, 4.0, col::FIRE_SOFT);
        }
        self.draw
            .text(mx + 14.0, y + 5.5, 10.5, col::FIRE, "Reset Default Layout");
        self.push_hit(mx + 4.0, y, mw - 8.0, row_h, Action::ResetLayout, "pointer");
        self.draw.end_overlay();
    }

    fn render_hover_card(&mut self, css_w: f32, css_h: f32, target: HoverTarget) {
        let title;
        let mut subtitle = String::new();
        let mut metrics = String::new();
        let mut badge_txt = String::new();
        let mut badge_col = col::FIRE;
        let mut code_key: Option<(String, u32)> = None;
        let mut extra_lines: Vec<String> = Vec::new();

        match target {
            HoverTarget::Bar(bar) => match bar.kind {
                BarKind::Span => {
                    title = self.world.fn_name(bar.slot_or_op);
                    if let Some(&fe) = self.world.slot_entity.get(&bar.slot_or_op) {
                        if let Some(f) = self.world.fns.get(fe) {
                            let self_ns = f.total_ns.saturating_sub(f.child_ns);
                            metrics = format!(
                                "dur {} · self {} · P95 {} · {}×",
                                fmt_ns(bar.dur_ns),
                                fmt_ns(self_ns),
                                fmt_ns(percentile(&f.samples, 0.95)),
                                f.calls
                            );
                            if !f.file.is_empty() && f.line > 0 {
                                subtitle = format!("{}:{}", short_path(&f.file), f.line);
                                code_key = Some((f.file.clone(), f.line));
                            }
                        }
                    }
                    if bar.cpu != CPU_UNKNOWN {
                        let is_e = (bar.flags & 0x10) != 0;
                        badge_txt = format!("Core #{} · {}", bar.cpu, if is_e { "E" } else { "P" });
                        badge_col = if is_e { col::SERIES2 } else { col::OK };
                    }
                }
                BarKind::Lock => {
                    title = "pthread_mutex_lock stall".into();
                    badge_txt = "LOCK WAIT".into();
                    badge_col = col::DANGER;
                    metrics = format!("wait {} · waiter Thread #{}", fmt_ns(bar.dur_ns), bar.thread);
                    if bar.holder_thread > 0 {
                        subtitle = format!("Blocked by Thread #{} @ 0x{:x}", bar.holder_thread, bar.holder_pc);
                    }
                    extra_lines.push(format!("mutex 0x{:x} · waiter pc 0x{:x}", bar.mutex_ptr, bar.waiter_pc));
                }
                BarKind::Heap => {
                    title = format!("Heap alloc · {}", fmt_bytes(bar.bytes as i64));
                    badge_txt = format!("Thread #{}", bar.thread);
                    badge_col = col::SERIES2;
                    metrics = format!("ptr 0x{:x} · caller 0x{:x}", bar.mutex_ptr, bar.holder_pc);
                }
                BarKind::Io => {
                    title = format!("I/O {}", io_op_name(bar.slot_or_op));
                    badge_txt = format!("Thread #{}", bar.thread);
                    badge_col = col::WARN;
                    metrics = format!("duration {} · fd {}", fmt_ns(bar.dur_ns), bar.arg);
                }
            },
            HoverTarget::ThreadGutter(tid) => {
                title = format!("Thread #{tid}");
                badge_col = col::SERIES2;
                if let Some(&te) = self.world.thread_entity.get(&tid) {
                    if let Some(tc) = self.world.threads.get(te) {
                        if tc.last_cpu != CPU_UNKNOWN {
                            let is_e = (tc.last_flags & 0x10) != 0;
                            badge_txt = format!("Core #{} ({})", tc.last_cpu, if is_e { "E-core" } else { "P-core" });
                        }
                        subtitle = if tc.parent_tid > 0 {
                            format!("Spawned by Thread #{}", tc.parent_tid)
                        } else {
                            "Process main thread".into()
                        };
                        metrics = format!("{} core migrations", tc.migrations);
                        if tc.spawn_frames[0] != 0 {
                            extra_lines.push(format!(
                                "spawn: 0x{:x} -> 0x{:x} -> 0x{:x}",
                                tc.spawn_frames[0], tc.spawn_frames[1], tc.spawn_frames[2]
                            ));
                        }
                    }
                }
            }
            HoverTarget::Flame(name, tot, slf, calls, root_tot) => {
                title = name;
                let pct = (tot as f64 / root_tot.max(1) as f64) * 100.0;
                let spct = (slf as f64 / root_tot.max(1) as f64) * 100.0;
                badge_txt = format!("{pct:.1}%");
                badge_col = col::FIRE;
                metrics = format!("total {} · self {} ({spct:.1}%) · {calls}×", fmt_ns(tot), fmt_ns(slf));
            }
        }

        if let Some((ref file, line)) = code_key {
            self.queue_source_if_needed(file, line);
        }
        let code_lines: Option<Vec<(u32, String)>> = code_key
            .as_ref()
            .and_then(|k| self.world.sources.get(k))
            .map(|v| v.iter().take(5).cloned().collect());

        let has_code = code_lines.as_ref().map(|v| !v.is_empty()).unwrap_or(false);
        let card_w = if has_code { 420.0 } else { 320.0 };
        let zone1_h = if subtitle.is_empty() { 48.0 } else { 62.0 } + (extra_lines.len() as f32) * 15.0;
        let zone2_h = if let Some(ref cl) = code_lines {
            if cl.is_empty() { 0.0 } else { 14.0 + cl.len() as f32 * 15.0 }
        } else {
            0.0
        };
        let card_h = zone1_h + zone2_h;

        let mut cx = self.mouse_x + 14.0;
        let mut cy = self.mouse_y + 14.0;
        if cx + card_w > css_w - 8.0 {
            cx = (self.mouse_x - card_w - 14.0).max(8.0);
        }
        if cy + card_h > css_h - 8.0 {
            cy = (self.mouse_y - card_h - 14.0).max(TOPBAR_H + 4.0);
        }

        self.draw.begin_overlay();
        self.draw
            .rrect(cx + 3.0, cy + 5.0, card_w, card_h, 8.0, [0.0, 0.0, 0.0, 0.55]);
        self.draw
            .card(cx, cy, card_w, card_h, 8.0, [0.068, 0.068, 0.074, 0.99], col::LINE2);

        let mut y = cy + 9.0;
        let bw = if !badge_txt.is_empty() {
            let w = self.draw.measure_text(&badge_txt, 9.0) + 12.0;
            self.draw.badge(
                cx + card_w - w - 10.0,
                y - 1.0,
                16.0,
                9.0,
                &badge_txt,
                badge_col,
                col::BG3,
                col::with_alpha(badge_col, 0.45),
            );
            w + 16.0
        } else {
            10.0
        };
        self.draw
            .text_max(cx + 11.0, y, 11.2, col::INK1, &title, card_w - bw - 14.0);
        y += 16.0;
        if !subtitle.is_empty() {
            self.draw
                .text_max(cx + 11.0, y, 9.8, col::INK3, &subtitle, card_w - 22.0);
            y += 14.0;
        }
        self.draw
            .text_max(cx + 11.0, y, 10.0, col::INK2, &metrics, card_w - 22.0);
        y += 15.0;
        for line in &extra_lines {
            self.draw
                .text_max(cx + 11.0, y, 9.8, col::INK3, line, card_w - 22.0);
            y += 15.0;
        }

        if let (Some(cl), Some((_, target_line))) = (code_lines, code_key) {
            if !cl.is_empty() {
                let z2_y = cy + zone1_h;
                self.draw.rect(cx + 1.0, z2_y, card_w - 2.0, 1.0, col::LINE);
                self.draw.rrect(
                    cx + 1.0,
                    z2_y + 1.0,
                    card_w - 2.0,
                    zone2_h - 2.0,
                    7.0,
                    [0.050, 0.050, 0.055, 0.99],
                );
                let mut ly = z2_y + 6.0;
                for (n, text) in cl {
                    let is_t = n == target_line;
                    if is_t {
                        self.draw.rect(cx + 1.0, ly - 1.0, card_w - 2.0, 15.0, col::FIRE_SOFT);
                    }
                    self.draw.text(
                        cx + 8.0,
                        ly + 1.0,
                        9.5,
                        if is_t { col::FIRE } else { col::INK4 },
                        &format!("{n:>4}"),
                    );
                    self.draw
                        .rust_code_line(cx + 38.0, ly + 1.0, 9.8, card_w - 46.0, &text);
                    ly += 15.0;
                }
            }
        }
        self.draw.end_overlay();
    }
}

fn pane_icon(pane: PaneId) -> Icon {
    match pane {
        PaneId::Overview => Icon::Pulse,
        PaneId::Timeline => Icon::Timeline,
        PaneId::FlameGraph => Icon::Flame,
        PaneId::CodeMap => Icon::Graph,
        PaneId::CpuTopology => Icon::Cpu,
        PaneId::HotSpots => Icon::Bolt,
        PaneId::HeapDiff => Icon::Memory,
        PaneId::Builds => Icon::Layers,
        PaneId::Advisor => Icon::Flame,
        PaneId::IoLocks => Icon::Lock,
        PaneId::Inspector => Icon::Code,
    }
}

fn fmt_elapsed_tick(elapsed_ms: f64, step_ms: f64) -> String {
    let sign = if elapsed_ms < -0.0005 { "-" } else { "" };
    let abs = elapsed_ms.abs();
    if abs < 0.0005 {
        return "0 s".to_string();
    }
    if abs < 1000.0 {
        if step_ms < 1.0 {
            format!("{sign}{abs:.2} ms")
        } else if step_ms < 10.0 {
            format!("{sign}{abs:.1} ms")
        } else {
            format!("{sign}{abs:.0} ms")
        }
    } else if abs < 60_000.0 {
        let secs = abs / 1000.0;
        if step_ms < 1.0 {
            format!("{sign}{secs:.4} s")
        } else if step_ms < 10.0 {
            format!("{sign}{secs:.3} s")
        } else if step_ms < 100.0 {
            format!("{sign}{secs:.2} s")
        } else if step_ms < 1000.0 {
            format!("{sign}{secs:.1} s")
        } else {
            format!("{sign}{secs:.0} s")
        }
    } else {
        let mins = (abs / 60_000.0).floor() as u64;
        let rem_s = (abs - (mins as f64) * 60_000.0) / 1000.0;
        if step_ms < 100.0 {
            format!("{sign}{mins}m {rem_s:05.2}s")
        } else if step_ms < 1000.0 {
            format!("{sign}{mins}m {rem_s:04.1}s")
        } else {
            format!("{sign}{mins}m {rem_s:02.0}s")
        }
    }
}

fn nice_step_ms(span_ms: f64, plot_w: f32) -> f64 {
    let target_ticks = (plot_w as f64 / 78.0).clamp(2.0, 9.0);
    let raw = span_ms / target_ticks;
    let pow = 10.0_f64.powf(raw.max(0.0001).log10().floor());
    let norm = raw / pow;
    if norm <= 1.0 {
        pow
    } else if norm <= 2.0 {
        2.0 * pow
    } else if norm <= 5.0 {
        5.0 * pow
    } else {
        10.0 * pow
    }
}

fn find_flame<'a>(node: &'a FlameNode, name: &str) -> Option<&'a FlameNode> {
    if node.name == name {
        return Some(node);
    }
    for ch in &node.children {
        if let Some(hit) = find_flame(ch, name) {
            return Some(hit);
        }
    }
    None
}

fn hash_flame_color(name: &str) -> col::Rgba {
    let mut h: u32 = 2166136261;
    for b in name.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(16777619);
    }
    const PALETTE: [col::Rgba; 6] = [
        [0.165, 0.435, 0.722, 0.92],
        [0.106, 0.541, 0.392, 0.92],
        [0.231, 0.475, 0.722, 0.92],
        [0.137, 0.494, 0.427, 0.92],
        [0.486, 0.361, 0.749, 0.92],
        [0.333, 0.412, 0.502, 0.92],
    ];
    PALETTE[(h as usize) % PALETTE.len()]
}
