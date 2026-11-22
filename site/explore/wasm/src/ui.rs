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
    VScrollSource {
        track_y: f32,
        track_h: f32,
        thumb_h: f32,
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
    HeapRangeSelect {
        plot_x: f32,
        plot_w: f32,
        t_min: f64,
        t_max: f64,
        anchor_t: f64,
    },
}

#[derive(Clone, Copy)]
struct HeapChartHit {
    rect: Rect,
    t_min: f64,
    t_max: f64,
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
    ClearHeapWindow,
    JumpToTraceTime(f64),
    OpenUrl(String),
    JumpSource(String, u32),
    SourceBack,
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
    Core(usize),
    Anomaly(Insight),
    HeapPoint {
        ts_ms: f64,
        retained: u64,
        alloc_bytes: u64,
        slope: f64,
    },
    StallPoint {
        ts_ms: f64,
        lock_ns: u64,
        io_ns: u64,
    },
    Port(crate::model::SocketInfo),
    MapCard(u32),
    HeapSite(crate::model::HeapSiteComp),
    CodeSym(crate::rust_ide::SymDef),
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
    pub heap_window: Option<(f64, f64)>,

    // Per-pane independent scroll positions & limits
    pane_scroll: HashMap<PaneId, f32>,
    pane_max_scroll: HashMap<PaneId, f32>,
    source_scroll_x: f32,
    source_max_scroll_x: f32,
    source_scroll_y: f32,
    source_max_scroll_y: f32,
    source_box_rect: Rect,
    source_auto_center: Option<(String, u32)>,

    // Active inspection & Post-IDE navigation state
    pub inspected_slot: Option<u32>,
    pub code_view: Option<(String, u32)>,
    pub code_history: Vec<(String, u32)>,
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
    vscroll_source: Option<VScrollHit>,
    hscroll_source: Option<HScrollHit>,
    minimap_hit: Option<MinimapHit>,
    radar_hit: Option<RadarHit>,
    heap_chart_hit: Option<HeapChartHit>,

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
            heap_window: None,
            pane_scroll: HashMap::new(),
            pane_max_scroll: HashMap::new(),
            source_scroll_x: 0.0,
            source_max_scroll_x: 0.0,
            source_scroll_y: 0.0,
            source_max_scroll_y: 0.0,
            source_box_rect: Rect::default(),
            source_auto_center: None,
            inspected_slot: None,
            code_view: None,
            code_history: Vec::new(),
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
            vscroll_source: None,
            hscroll_source: None,
            minimap_hit: None,
            radar_hit: None,
            heap_chart_hit: None,
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
        if self.world.file_lines.contains_key(file) || self.world.pending_files.contains(file) {
            return;
        }
        let key = (file.to_string(), line);
        if self.world.sources.contains_key(&key) || self.world.pending_sources.contains(&key) {
            return;
        }
        self.world.pending_files.insert(file.to_string());
        self.world.pending_sources.insert(key);
        self.commands.push(format!("source:{file}|{line}"));
    }

    fn def_cache_key(from_file: &str, q: &crate::rust_ide::CrossFileQuery) -> String {
        format!(
            "{}|{}|{}|{}|{}",
            from_file,
            q.prefix.as_deref().unwrap_or(""),
            q.import_path.as_deref().unwrap_or(""),
            q.want_kind,
            q.ident
        )
    }

    fn queue_def_if_needed(&mut self, from_file: &str, q: &crate::rust_ide::CrossFileQuery) {
        if q.ident.is_empty() {
            return;
        }
        let key = Self::def_cache_key(from_file, q);
        if self.world.workspace_defs.contains_key(&key) || self.world.pending_defs.contains(&key) {
            return;
        }
        self.world.pending_defs.insert(key.clone());
        self.commands.push(format!(
            "def:{}\t{}\t{}\t{}\t{}\t{}",
            key,
            q.ident,
            from_file,
            q.prefix.as_deref().unwrap_or(""),
            q.import_path.as_deref().unwrap_or(""),
            q.want_kind
        ));
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
            DragOp::VScrollSource {
                track_y,
                track_h,
                thumb_h,
                max_scroll,
                grab_off,
            } => {
                let travel = (track_h - thumb_h).max(1.0);
                let frac = ((y - grab_off - track_y) / travel).clamp(0.0, 1.0);
                self.source_scroll_y = (frac * max_scroll).clamp(0.0, max_scroll);
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
            DragOp::HeapRangeSelect {
                plot_x,
                plot_w,
                t_min,
                t_max,
                anchor_t,
            } => {
                let norm = (((x - plot_x) / plot_w.max(1.0)) as f64).clamp(0.0, 1.0);
                let cur_t = t_min + norm * (t_max - t_min).max(1.0);
                let (w0, w1) = if cur_t >= anchor_t {
                    (anchor_t, cur_t)
                } else {
                    (cur_t, anchor_t)
                };
                self.heap_window = Some((w0, w1));
                if w1 - w0 >= 50.0 {
                    self.dock.open_split(PaneId::HeapDiff, SplitDir::Vertical, 0.54);
                }
                self.desired_cursor = "col-resize";
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
            || self.vscroll_source.map(|v| v.track.contains(x, y)).unwrap_or(false)
            || self.hscroll_source.map(|h| h.track.contains(x, y)).unwrap_or(false)
            || self.radar_hit.map(|r| r.rect.contains(x, y)).unwrap_or(false)
        {
            self.desired_cursor = "pointer";
        } else if self.heap_chart_hit.map(|h| h.rect.contains(x, y)).unwrap_or(false) {
            self.desired_cursor = "crosshair";
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
        if let Some(vs) = self.vscroll_source {
            if vs.track.contains(x, y) {
                let grab_off = if vs.thumb.contains(x, y) {
                    y - vs.thumb.y
                } else {
                    let travel = (vs.track.h - vs.thumb.h).max(1.0);
                    let frac = ((y - vs.thumb.h * 0.5 - vs.track.y) / travel).clamp(0.0, 1.0);
                    self.source_scroll_y = (frac * vs.max_scroll).clamp(0.0, vs.max_scroll);
                    vs.thumb.h * 0.5
                };
                self.drag_op = DragOp::VScrollSource {
                    track_y: vs.track.y,
                    track_h: vs.track.h,
                    thumb_h: vs.thumb.h,
                    max_scroll: vs.max_scroll,
                    grab_off,
                };
                return;
            }
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
        if let Some(hc) = self.heap_chart_hit {
            if hc.rect.contains(x, y) {
                let norm = (((x - hc.rect.x) / hc.rect.w.max(1.0)) as f64).clamp(0.0, 1.0);
                let anchor_t = hc.t_min + norm * (hc.t_max - hc.t_min).max(1.0);
                self.drag_op = DragOp::HeapRangeSelect {
                    plot_x: hc.rect.x,
                    plot_w: hc.rect.w,
                    t_min: hc.t_min,
                    t_max: hc.t_max,
                    anchor_t,
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
            DragOp::HeapRangeSelect { .. } => {
                if moved < 5.0 {
                    // Plain click on HEAP chart opens/splits Heap pane cleanly (never jumps to Trace!)
                    self.heap_window = None;
                } else if let Some((t0, t1)) = self.heap_window {
                    if t1 - t0 < 50.0 {
                        self.heap_window = None;
                    }
                }
                self.dock.open_split(PaneId::HeapDiff, SplitDir::Vertical, 0.54);
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
                if self.source_box_rect.contains(x, y) {
                    if dy.abs() > 0.25 && self.source_max_scroll_y > 0.0 {
                        self.source_scroll_y = (self.source_scroll_y + dy).clamp(0.0, self.source_max_scroll_y);
                    } else if dy.abs() > 0.25 {
                        let cur = self.scroll_of(PaneId::Inspector);
                        self.set_scroll(PaneId::Inspector, cur + dy);
                    }
                    if dx.abs() > 0.25 && self.source_max_scroll_x > 0.0 {
                        self.source_scroll_x = (self.source_scroll_x + dx).clamp(0.0, self.source_max_scroll_x);
                    }
                } else {
                    if dy.abs() > 0.25 {
                        let cur = self.scroll_of(PaneId::Inspector);
                        self.set_scroll(PaneId::Inspector, cur + dy);
                    }
                    if dx.abs() > 0.25 && self.source_max_scroll_x > 0.0 {
                        self.source_scroll_x = (self.source_scroll_x + dx).clamp(0.0, self.source_max_scroll_x);
                    }
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
                self.code_view = None;
                self.source_scroll_x = 0.0;
                if let Some(&fe) = self.world.slot_entity.get(&slot) {
                    if let Some(f) = self.world.fns.get(fe) {
                        if !f.file.is_empty() && f.line > 0 {
                            self.source_auto_center = Some((f.file.clone(), f.line));
                        }
                    }
                }
                self.dock.focus_or_open(PaneId::Inspector);
            }
            Action::JumpSource(file, line) => {
                // Save current location into navigation history for `← Back`
                if let Some(cur) = self.current_code_loc() {
                    if cur != (file.clone(), line) {
                        self.code_history.push(cur);
                        if self.code_history.len() > 32 {
                            self.code_history.remove(0);
                        }
                    }
                }
                self.code_view = Some((file.clone(), line));
                self.source_scroll_x = 0.0;
                self.source_auto_center = Some((file.clone(), line));
                self.queue_source_if_needed(&file, line);
                self.dock.focus_or_open(PaneId::Inspector);
            }
            Action::SourceBack => {
                if let Some((file, line)) = self.code_history.pop() {
                    self.code_view = Some((file.clone(), line));
                    self.source_scroll_x = 0.0;
                    self.source_auto_center = Some((file.clone(), line));
                    self.queue_source_if_needed(&file, line);
                } else {
                    self.code_view = None;
                }
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
            Action::ClearHeapWindow => self.heap_window = None,
            Action::JumpToTraceTime(ts_ms) => {
                self.view_span_ms = 1_500.0;
                self.view_end_ms = ts_ms + 750.0;
                self.following = false;
                self.dock.focus_or_open(PaneId::Timeline);
            }
            Action::OpenUrl(url) => {
                self.commands.push(format!("open:{url}"));
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
        self.vscroll_source = None;
        self.hscroll_source = None;
        self.source_box_rect = Rect::default();
        self.minimap_hit = None;
        self.radar_hit = None;
        self.heap_chart_hit = None;
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
            x += self.draw
                .text(x, cy - 6.8, 11.0, col::INK2, &format!("{} ev", self.world.total_events))
                + 12.0;
        }
        // Live listening port chips right in the topbar (1-click opens `http://127.0.0.1:<port>`)
        let listen_ports: Vec<u16> = self
            .world
            .resources
            .sockets
            .iter()
            .filter(|s| s.state.eq_ignore_ascii_case("LISTEN") && s.port > 0)
            .map(|s| s.port)
            .take(3)
            .collect();
        for port in listen_ports {
            let lbl = format!(":{port}");
            let pw = self.draw.measure_text(&lbl, 10.0) + 18.0;
            if x + pw > max_left_x + 120.0 {
                break;
            }
            let hov = self.is_hovered(x, 9.0, pw, 20.0);
            self.draw.card(
                x,
                9.0,
                pw,
                20.0,
                4.0,
                if hov { col::OK_SOFT } else { col::BG2 },
                col::with_alpha(col::OK, if hov { 0.85 } else { 0.42 }),
            );
            self.draw.circle(x + 7.0, cy, 2.5, col::OK);
            self.draw.text(x + 13.0, cy - 6.0, 10.0, col::OK, &lbl);
            self.push_hit(x, 9.0, pw, 20.0, Action::OpenUrl(format!("http://127.0.0.1:{port}")), "pointer");
            x += pw + 5.0;
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
    // Pane: Vitals (Automatic Anomaly Radar + Per-Core Silicon Heatmap + Open Ports + Heap/Stalls)
    // -----------------------------------------------------------------------

    fn render_overview_pane(&mut self, area: Rect) {
        let pad = 10.0;
        let gap = 8.0;
        let scroll = self.scroll_of(PaneId::Overview);
        let avail_w = (area.w - pad * 2.0 - 6.0).max(260.0);
        let x0 = area.x + pad;
        let y_top = area.y + 8.0 - scroll;
        let two_col = avail_w >= 660.0;

        let insights = self.world.analyze_insights();
        let buckets = self.world.vitals.clone();
        let origin = self
            .world
            .trace_origin_ms
            .unwrap_or_else(|| buckets.first().map(|b| b.ts_ms).unwrap_or(0.0));

        let left_w = if two_col { (avail_w * 0.50).floor() } else { avail_w };
        let right_x = if two_col { x0 + left_w + gap } else { x0 };
        let right_w = if two_col { avail_w - left_w - gap } else { avail_w };

        // ---- LEFT COLUMN: Automatic Anomaly Radar + Live Open Ports & Sockets ----
        let mut ly = y_top;

        // 1. Live Automatic Anomalies Card (finds weird things automatically, 1-click to culprit!)
        let anom_hdr_h = 26.0;
        let anom_row_h = 30.0;
        let shown = insights.len().min(6);
        let anom_body_h = if shown == 0 { 42.0 } else { shown as f32 * anom_row_h };
        let anom_h = anom_hdr_h + anom_body_h;

        let crit_cnt = insights.iter().filter(|i| i.severity == 2).count();
        let hdr_col = if crit_cnt > 0 {
            col::DANGER
        } else if !insights.is_empty() {
            col::WARN
        } else {
            col::OK
        };

        self.draw.card(x0, ly, left_w, anom_h, 7.0, col::BG2, col::LINE);
        self.draw.circle(x0 + 13.0, ly + 13.0, 3.2, hdr_col);
        self.draw.text(x0 + 23.0, ly + 7.5, 10.0, col::INK2, "ANOMALIES");
        let status_lbl = if insights.is_empty() {
            "ALL CLEAR".to_string()
        } else {
            format!("{} ACTIVE", insights.len())
        };
        let slw = self.draw.measure_text(&status_lbl, 9.5);
        self.draw
            .text(x0 + left_w - slw - 10.0, ly + 8.0, 9.5, hdr_col, &status_lbl);
        self.draw
            .rect(x0, ly + anom_hdr_h, left_w, 1.0, col::LINE);

        if insights.is_empty() {
            self.draw.text(
                x0 + 14.0,
                ly + anom_hdr_h + 13.0,
                10.5,
                col::OK,
                "No leaks, lock stalls, E-core traps, or tail spikes detected",
            );
        } else {
            for (i, ins) in insights.iter().take(6).enumerate() {
                let ry = ly + anom_hdr_h + i as f32 * anom_row_h;
                if i > 0 {
                    self.draw.rect(x0, ry, left_w, 1.0, col::LINE);
                }
                let (fg, bg) = match ins.severity {
                    2 => (col::DANGER, col::DANGER_SOFT),
                    1 => (col::WARN, col::FIRE_SOFT),
                    _ => (col::OK, col::OK_SOFT),
                };
                let ico = insight_icon(ins.category);
                let hov = self.is_hovered(x0 + 1.0, ry, left_w - 2.0, anom_row_h);
                if hov {
                    self.draw
                        .rect(x0 + 1.0, ry + 1.0, left_w - 2.0, anom_row_h - 1.0, [1.0, 1.0, 1.0, 0.035]);
                    self.hover_target = Some(HoverTarget::Anomaly(ins.clone()));
                }
                self.draw
                    .card(x0 + 8.0, ry + 5.0, 20.0, 20.0, 5.0, bg, col::with_alpha(fg, 0.45));
                self.draw.icon(ico, x0 + 12.0, ry + 9.0, 12.0, fg);

                let mw = self.draw.measure_text(&ins.metric, 9.5) + 12.0;
                let badge_x = x0 + left_w - mw - 10.0;
                let title_w = self.draw.text_max(
                    x0 + 36.0,
                    ry + 8.5,
                    10.5,
                    col::INK1,
                    &ins.title,
                    (badge_x - (x0 + 44.0)).max(60.0),
                );
                let det_x = x0 + 36.0 + title_w + 8.0;
                if badge_x - det_x >= 44.0 {
                    self.draw.text_max(
                        det_x,
                        ry + 9.0,
                        9.5,
                        col::INK3,
                        &ins.detail,
                        badge_x - det_x - 8.0,
                    );
                }
                self.draw
                    .badge(badge_x, ry + 6.0, 18.0, 9.5, &ins.metric, fg, bg, col::with_alpha(fg, 0.4));

                let act = insight_action(ins);
                self.push_hit(x0, ry, left_w, anom_row_h, act, "pointer");
            }
        }
        ly += anom_h + gap;

        // 2. Live Open Ports, Active Sockets & FDs Card (`bstd` / OS introspection!)
        let res = &self.world.resources;
        let listens: Vec<_> = res
            .sockets
            .iter()
            .filter(|s| s.state.eq_ignore_ascii_case("LISTEN") && s.port > 0)
            .cloned()
            .collect();
        let conns: Vec<_> = res
            .sockets
            .iter()
            .filter(|s| !s.state.eq_ignore_ascii_case("LISTEN") && !s.remote.is_empty())
            .cloned()
            .collect();
        let net_h = 74.0 + if !conns.is_empty() { 20.0 } else { 0.0 };
        self.draw.card(x0, ly, left_w, net_h, 7.0, col::BG2, col::LINE);
        self.draw.icon(Icon::Layers, x0 + 10.0, ly + 8.0, 12.0, col::OK);
        self.draw.text(x0 + 26.0, ly + 7.5, 10.0, col::INK2, "PORTS & FDS");
        let fd_txt = format!("{} FDs · {} sockets", res.fd_count, res.sockets.len());
        let fdw = self.draw.measure_text(&fd_txt, 9.5);
        self.draw
            .text(x0 + left_w - fdw - 10.0, ly + 8.0, 9.5, col::INK3, &fd_txt);
        self.draw.rect(x0, ly + 25.0, left_w, 1.0, col::LINE);

        let mut px = x0 + 10.0;
        let py_ports = ly + 34.0;
        if listens.is_empty() {
            self.draw.text(
                px,
                py_ports + 4.0,
                10.0,
                col::INK3,
                "No listening ports bound by this process",
            );
        } else {
            for s in listens.iter().take(6) {
                let lbl = format!("{} :{}", s.proto, s.port);
                let pw = self.draw.measure_text(&lbl, 10.0) + 22.0;
                if px + pw > x0 + left_w - 8.0 {
                    break;
                }
                let hov = self.is_hovered(px, py_ports, pw, 22.0);
                if hov {
                    self.hover_target = Some(HoverTarget::Port(s.clone()));
                }
                self.draw.card(
                    px,
                    py_ports,
                    pw,
                    22.0,
                    5.0,
                    if hov { col::OK_SOFT } else { col::BG3 },
                    col::with_alpha(col::OK, if hov { 0.9 } else { 0.45 }),
                );
                self.draw.circle(px + 8.0, py_ports + 11.0, 2.8, col::OK);
                self.draw.text(px + 15.0, py_ports + 5.0, 10.0, col::INK1, &lbl);
                self.push_hit(
                    px,
                    py_ports,
                    pw,
                    22.0,
                    Action::OpenUrl(format!("http://127.0.0.1:{}", s.port)),
                    "pointer",
                );
                px += pw + 6.0;
            }
        }
        if let Some(first_c) = conns.first() {
            let cy = ly + 64.0;
            self.draw.text_max(
                x0 + 10.0,
                cy,
                9.5,
                col::INK3,
                &format!(
                    "{} {} → {} ({}){}",
                    first_c.proto,
                    first_c.local,
                    first_c.remote,
                    first_c.state,
                    if conns.len() > 1 {
                        format!("  +{} more", conns.len() - 1)
                    } else {
                        String::new()
                    }
                ),
                left_w - 20.0,
            );
        }
        ly += net_h + gap;

        // 3. TOP FUNCTIONS card (live top 5 self-time hotspots right in Vitals — click -> Code!)
        let mut hot_fns: Vec<FnComp> = self.world.fns.iter().map(|(_, f)| f.clone()).collect();
        hot_fns.sort_by(|a, b| {
            let sa = a.total_ns.saturating_sub(a.child_ns);
            let sb = b.total_ns.saturating_sub(b.child_ns);
            sb.cmp(&sa)
        });
        let total_self_all: u64 = hot_fns
            .iter()
            .map(|f| f.total_ns.saturating_sub(f.child_ns))
            .sum::<u64>()
            .max(1);
        let hot_rows = hot_fns.len().clamp(1, 5);
        let hot_row_h = 30.0;
        let hot_h = 26.0 + hot_rows as f32 * hot_row_h;
        self.draw.card(x0, ly, left_w, hot_h, 7.0, col::BG2, col::LINE);
        self.draw.icon(Icon::Flame, x0 + 10.0, ly + 8.0, 12.0, col::FIRE);
        self.draw.text(x0 + 26.0, ly + 7.5, 10.0, col::INK2, "TOP FUNCTIONS");
        let all_hot_w = self.draw.measure_text("All Hot →", 9.5);
        let ah_x = x0 + left_w - all_hot_w - 10.0;
        let ah_hov = self.is_hovered(ah_x - 4.0, ly + 4.0, all_hot_w + 8.0, 18.0);
        self.draw
            .text(ah_x, ly + 8.0, 9.5, if ah_hov { col::FIRE } else { col::INK3 }, "All Hot →");
        self.push_hit(ah_x - 4.0, ly + 4.0, all_hot_w + 8.0, 18.0, Action::OpenPane(PaneId::HotSpots), "pointer");
        self.draw.rect(x0, ly + 25.0, left_w, 1.0, col::LINE);

        if hot_fns.is_empty() {
            self.draw
                .text(x0 + 12.0, ly + 33.0, 10.2, col::INK3, "Waiting for function spans…");
        } else {
            for (i, f) in hot_fns.iter().take(5).enumerate() {
                let ry = ly + 26.0 + i as f32 * hot_row_h;
                if i > 0 {
                    self.draw.rect(x0, ry, left_w, 1.0, col::LINE);
                }
                let hov = self.is_hovered(x0, ry, left_w, hot_row_h);
                if hov {
                    self.draw
                        .rect(x0 + 1.0, ry + 1.0, left_w - 2.0, hot_row_h - 1.0, [1.0, 1.0, 1.0, 0.04]);
                    self.hover_target = Some(HoverTarget::MapCard(f.slot));
                }
                self.push_hit(x0, ry, left_w, hot_row_h, Action::InspectSlot(f.slot), "pointer");

                let self_ns = f.total_ns.saturating_sub(f.child_ns);
                let pct = (self_ns as f64 / total_self_all as f64).clamp(0.0, 1.0) as f32;
                let p95_s = format!("P95 {}", fmt_ns(percentile(&f.samples, 0.95)));
                let pct_s = format!("{:.0}%", pct * 100.0);
                let p95_w = self.draw.measure_text(&p95_s, 9.5);
                let pct_w = self.draw.measure_text(&pct_s, 9.8);
                let bar_w = if left_w >= 420.0 { 68.0 } else { 42.0 };
                let right_start = x0 + left_w - p95_w - pct_w - bar_w - 30.0;

                self.draw.text_max(
                    x0 + 10.0,
                    ry + 8.5,
                    10.2,
                    col::INK1,
                    &f.name,
                    (right_start - x0 - 18.0).max(60.0),
                );
                let bx = right_start;
                self.draw.rrect(bx, ry + 13.0, bar_w, 4.5, 2.0, col::BG4);
                self.draw.rrect(
                    bx,
                    ry + 13.0,
                    (bar_w * pct).clamp(3.0, bar_w),
                    4.5,
                    2.0,
                    if pct >= 0.25 { col::FIRE } else { col::SERIES2 },
                );
                self.draw
                    .text(bx + bar_w + 8.0, ry + 9.0, 9.8, col::INK1, &pct_s);
                self.draw
                    .text(x0 + left_w - p95_w - 10.0, ry + 9.0, 9.5, col::INK3, &p95_s);
            }
        }
        ly += hot_h;

        // ---- RIGHT COLUMN: Live Per-Core Silicon Heatmap + Retained Heap + Stalls + Top Allocators ----
        let mut ry = if two_col { y_top } else { ly + gap };

        // 4. Per-Core Silicon Heatmap (`P-CORES` | `E-CORES` live vertical thermal bars!)
        let core_h = 148.0;
        self.draw_per_core_heatmap(Rect {
            x: right_x,
            y: ry,
            w: right_w,
            h: core_h,
        });
        ry += core_h + gap;

        // 5. Mid-Right Pair: Retained Heap Leak Curve + Discrete Lock/I/O Stalls
        let pair_h = 144.0;
        let sub_cols = if right_w >= 380.0 { 2 } else { 1 };
        let sub_w = if sub_cols == 2 {
            (right_w - gap) * 0.5
        } else {
            right_w
        };
        let n = buckets.len();
        let mut hover_idx: Option<usize> = None;
        for ci in 0..2 {
            let cx = right_x + (ci % sub_cols) as f32 * (sub_w + gap);
            let cy = ry + (ci / sub_cols) as f32 * (pair_h + gap);
            if n > 0 && self.is_hovered(cx + 10.0, cy + 28.0, (sub_w - 20.0).max(20.0), (pair_h - 38.0).max(20.0)) {
                let frac = ((self.mouse_x - (cx + 10.0)) / (sub_w - 20.0).max(1.0)).clamp(0.0, 0.9999);
                hover_idx = Some(((frac * n as f32) as usize).min(n - 1));
                break;
            }
        }

        let ret_bytes = self.world.total_retained_bytes();
        let mem_slope = self.world.heap_slope_bps();
        let mem_vals: Vec<f64> = buckets.iter().map(|b| b.retained_bytes as f64).collect();
        let mem_max = mem_vals.iter().copied().fold(65536.0_f64, f64::max) * 1.15;
        let mem_sub = if mem_slope.abs() >= 1024.0 {
            format!("{}{}/s", if mem_slope > 0.0 { "+" } else { "" }, fmt_bytes(mem_slope as i64))
        } else {
            "stable".into()
        };
        self.draw_line_chart(
            Rect {
                x: right_x,
                y: ry,
                w: sub_w,
                h: pair_h,
            },
            Icon::Memory,
            "HEAP",
            &fmt_bytes(ret_bytes as i64),
            &mem_sub,
            if mem_slope > 32_768.0 { col::DANGER } else { col::SERIES2 },
            &mem_vals,
            mem_max,
            &buckets,
            origin,
            hover_idx,
        );

        let stall_rect = Rect {
            x: right_x + (1 % sub_cols) as f32 * (sub_w + gap),
            y: ry + (1 / sub_cols) as f32 * (pair_h + gap),
            w: sub_w,
            h: pair_h,
        };
        self.draw_stall_chart(stall_rect, &buckets, origin, hover_idx);
        ry += if sub_cols == 2 { pair_h } else { pair_h * 2.0 + gap } + gap;

        // 6. TOP ALLOCATORS card (live top 4 heap allocation sites right in Vitals — hover for Rust source peek!)
        let alloc_sites = self.world.heap_sites_for_window(self.heap_window);
        let a_rows = alloc_sites.len().clamp(1, 4);
        let a_row_h = 34.0;
        let a_h = 26.0 + a_rows as f32 * a_row_h;
        self.draw.card(right_x, ry, right_w, a_h, 7.0, col::BG2, col::LINE);
        self.draw.icon(Icon::Memory, right_x + 10.0, ry + 8.0, 12.0, col::SERIES2);
        let a_title = if self.heap_window.is_some() {
            "ALLOCATORS (WINDOW)"
        } else {
            "TOP ALLOCATORS"
        };
        self.draw.text(right_x + 26.0, ry + 7.5, 10.0, col::INK2, a_title);
        let open_hp_w = self.draw.measure_text("Inspect Heap →", 9.5);
        let oh_x = right_x + right_w - open_hp_w - 10.0;
        let oh_hov = self.is_hovered(oh_x - 4.0, ry + 4.0, open_hp_w + 8.0, 18.0);
        self.draw.text(
            oh_x,
            ry + 8.0,
            9.5,
            if oh_hov { col::SERIES2 } else { col::INK3 },
            "Inspect Heap →",
        );
        self.push_hit(
            oh_x - 4.0,
            ry + 4.0,
            open_hp_w + 8.0,
            18.0,
            Action::OpenPane(PaneId::HeapDiff),
            "pointer",
        );
        self.draw.rect(right_x, ry + 25.0, right_w, 1.0, col::LINE);

        if alloc_sites.is_empty() {
            self.draw
                .text(right_x + 12.0, ry + 35.0, 10.2, col::INK3, "No heap allocations recorded");
        } else {
            for (i, s) in alloc_sites.iter().take(4).enumerate() {
                let row_y = ry + 26.0 + i as f32 * a_row_h;
                if i > 0 {
                    self.draw.rect(right_x, row_y, right_w, 1.0, col::LINE);
                }
                let hov = self.is_hovered(right_x, row_y, right_w, a_row_h);
                if hov {
                    self.draw.rect(
                        right_x + 1.0,
                        row_y + 1.0,
                        right_w - 2.0,
                        a_row_h - 1.0,
                        [1.0, 1.0, 1.0, 0.04],
                    );
                    self.hover_target = Some(HoverTarget::HeapSite(s.clone()));
                }
                if let Some(slot) = s.slot {
                    self.push_hit(right_x, row_y, right_w, a_row_h, Action::InspectSlot(slot), "pointer");
                } else {
                    self.push_hit(
                        right_x,
                        row_y,
                        right_w,
                        a_row_h,
                        Action::OpenPane(PaneId::HeapDiff),
                        "pointer",
                    );
                }

                let live_cnt = s.allocs.saturating_sub(s.frees);
                let survival = if s.allocs > 0 {
                    live_cnt as f64 / s.allocs as f64
                } else {
                    0.0
                };
                let (st_lbl, st_fg, st_bg) = if s.allocs >= 4 && survival >= 0.85 && s.retained_bytes > 0 {
                    ("LEAK", col::DANGER, col::DANGER_SOFT)
                } else if s.retained_bytes > 0 && survival >= 0.45 {
                    ("GROWING", col::WARN, [0.85, 0.58, 0.19, 0.16])
                } else if s.allocs >= 6 && s.retained_bytes == 0 {
                    ("CHURN", col::SERIES2, col::SERIES2_SOFT)
                } else {
                    ("OK", col::OK, col::OK_SOFT)
                };

                let avg = if s.allocs > 0 { s.total_bytes / s.allocs } else { 0 };
                let right_txt = if s.retained_bytes > 0 {
                    format!("{} · {} live", fmt_bytes(s.retained_bytes as i64), live_cnt)
                } else {
                    format!("{} churn", fmt_bytes(s.total_bytes as i64))
                };
                let rw_txt = self.draw.measure_text(&right_txt, 9.8);
                let bw_badge = self.draw.measure_text(st_lbl, 8.6) + 12.0;
                let badge_x = right_x + right_w - rw_txt - bw_badge - 18.0;

                let name = if s.top_fn.is_empty() { s.key.as_str() } else { s.top_fn.as_str() };
                let name_w = self.draw.text_max(
                    right_x + 10.0,
                    row_y + 10.0,
                    10.2,
                    col::INK1,
                    name,
                    (badge_x - right_x - 16.0).max(60.0),
                );
                let sub = format!("avg {} · t#{}", fmt_bytes(avg as i64), s.thread);
                let sub_avail = badge_x - (right_x + 10.0 + name_w + 8.0) - 8.0;
                if sub_avail >= 48.0 {
                    self.draw
                        .text_max(right_x + 10.0 + name_w + 8.0, row_y + 10.5, 9.2, col::INK3, &sub, sub_avail);
                }

                self.draw.badge(
                    badge_x,
                    row_y + 8.5,
                    17.0,
                    8.6,
                    st_lbl,
                    st_fg,
                    st_bg,
                    col::with_alpha(st_fg, 0.45),
                );
                self.draw.text(
                    right_x + right_w - rw_txt - 10.0,
                    row_y + 10.5,
                    9.8,
                    if s.retained_bytes > 0 { col::INK1 } else { col::INK3 },
                    &right_txt,
                );
            }
        }
        ry += a_h;

        let content_h = (ly.max(ry) + scroll - area.y) + 12.0;
        self.draw_vscrollbar(PaneId::Overview, area.x + area.w - 9.0, area.y, area.h, content_h);
    }

    /// Live Per-Core Silicon Heatmap (`P-CORES` left, `E-CORES` right) — shows exact hardware balance!
    fn draw_per_core_heatmap(&mut self, r: Rect) {
        self.draw.card(r.x, r.y, r.w, r.h, 7.0, col::BG2, col::LINE);
        let top = &self.world.topology;
        let ncpu = top.ncpu.clamp(1, 32) as usize;
        let p_cores = (top.p_cores as usize).min(ncpu);
        let e_cores = ncpu.saturating_sub(p_cores);

        let px = r.x + 10.0;
        let py = r.y + 30.0;
        let pw = (r.w - 20.0).max(60.0);
        let ph = (r.h - 48.0).max(34.0);
        let has_split = p_cores > 0 && e_cores > 0;
        let cluster_gap = if has_split { 10.0 } else { 0.0 };
        let col_gap = 3.0;
        let tile_w = ((pw - cluster_gap - col_gap * (ncpu.saturating_sub(1) as f32)) / ncpu as f32).max(6.0);

        let mut hovered_core: Option<usize> = None;
        for c in 0..ncpu {
            let extra = if has_split && c >= p_cores { cluster_gap } else { 0.0 };
            let cx = px + c as f32 * (tile_w + col_gap) + extra;
            if self.is_hovered(cx, py, tile_w, ph + 14.0) {
                hovered_core = Some(c);
                self.hover_target = Some(HoverTarget::Core(c));
            }
        }

        // Header: Brand / Core hover readout
        self.draw.icon(Icon::Cpu, r.x + 10.0, r.y + 8.0, 12.0, col::FIRE);
        let hdr = if let Some(hc) = hovered_core {
            let is_e = has_split && hc >= p_cores;
            let pct = ((self.world.cpu_live_us[hc] / 500_000.0) * 100.0).clamp(0.0, 100.0);
            format!(
                "CORE {:02} ({}) · {:.0}% · {} ev",
                hc,
                if is_e { "E-core" } else { "P-core" },
                pct,
                self.world.cpu_events[hc]
            )
        } else {
            format!("{} · {}P+{}E", top.brand, p_cores, e_cores)
        };
        self.draw
            .text_max(r.x + 26.0, r.y + 7.5, 10.0, col::INK2, &hdr, r.w - 36.0);

        // Cluster divider + subtle labels
        if has_split {
            let div_x = px + p_cores as f32 * (tile_w + col_gap) + cluster_gap * 0.5 - col_gap * 0.5;
            self.draw.rect(div_x, py, 1.0, ph, col::LINE2);
        }

        // Draw every physical core as a live vertical thermal bar
        for c in 0..ncpu {
            let extra = if has_split && c >= p_cores { cluster_gap } else { 0.0 };
            let cx = px + c as f32 * (tile_w + col_gap) + extra;
            let is_e = has_split && c >= p_cores;
            let live_pct = (self.world.cpu_live_us[c] / 500_000.0).clamp(0.0, 1.0);
            let is_hov = hovered_core == Some(c);

            // Empty core well
            self.draw.rrect(cx, py, tile_w, ph, 3.0, col::BG4);
            if live_pct > 0.01 {
                let fill_h = (ph * live_pct).max(3.0);
                // P-cores glow fire-orange; E-cores glow blue (amber if hot work trapped on E-core!)
                let bar_col = if is_e {
                    if live_pct > 0.35 { col::WARN } else { col::SERIES2 }
                } else if live_pct > 0.75 {
                    col::DANGER
                } else {
                    col::FIRE
                };
                self.draw
                    .rrect(cx, py + ph - fill_h, tile_w, fill_h, 3.0, bar_col);
            }
            if is_hov {
                self.draw.border(cx, py, tile_w, ph, 3.0, 1.0, col::INK1);
            }

            // Core index label underneath (skip odd labels if very tight)
            if tile_w >= 14.0 || c % 2 == 0 {
                let lbl = format!("{c}");
                let lw = self.draw.measure_text(&lbl, 8.5);
                self.draw.text(
                    cx + (tile_w - lw) * 0.5,
                    py + ph + 4.0,
                    8.5,
                    if is_hov {
                        col::INK1
                    } else if is_e {
                        col::SERIES2
                    } else {
                        col::INK3
                    },
                    &lbl,
                );
            }
        }
        self.push_hit(px, py, pw, ph + 14.0, Action::OpenPane(PaneId::CpuTopology), "pointer");
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
        let tw_title = self.draw.text(r.x + 26.0, r.y + 7.5, 10.0, col::INK3, title);
        let title_end = r.x + 26.0 + tw_title + 8.0;

        let pw_txt = self.draw.measure_text(primary, 10.2);
        let sw_txt = self.draw.measure_text(sub, 9.2);
        let both_x = r.x + r.w - pw_txt - sw_txt - 16.0;
        if both_x >= title_end {
            self.draw.text(both_x, r.y + 7.2, 10.2, col::INK1, primary);
            self.draw
                .text(r.x + r.w - sw_txt - 10.0, r.y + 8.0, 9.2, color, sub);
        } else if r.x + r.w - pw_txt - 10.0 >= title_end {
            self.draw
                .text(r.x + r.w - pw_txt - 10.0, r.y + 7.2, 10.2, col::INK1, primary);
        }

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
        let wash = col::with_alpha(color, 0.12);

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

        // Register interactive drag-to-select window on HEAP chart (dragging filters Heap pane, clicking opens Heap!)
        if let (Some(first_b), Some(last_b)) = (buckets.first(), buckets.last()) {
            let t_min = first_b.ts_ms;
            let t_max = (last_b.ts_ms + 500.0).max(t_min + 500.0);
            self.heap_chart_hit = Some(HeapChartHit {
                rect: Rect {
                    x: px,
                    y: py,
                    w: pw,
                    h: ph,
                },
                t_min,
                t_max,
            });
            // Draw dragged memory window `[w0 .. w1]` right on the HEAP chart!
            if let Some((w0, w1)) = self.heap_window {
                let total = (t_max - t_min).max(1.0);
                let sx0 = px + (((w0 - t_min) / total).clamp(0.0, 1.0) as f32) * pw;
                let sx1 = px + (((w1 - t_min) / total).clamp(0.0, 1.0) as f32) * pw;
                if sx1 > sx0 + 1.0 {
                    self.draw.rect(sx0, py, sx1 - sx0, ph, col::SERIES2_SOFT);
                    self.draw.rect(sx0, py, 1.0, ph, col::SERIES2);
                    self.draw.rect(sx1, py, 1.0, ph, col::SERIES2);
                }
            }
        }

        if let Some(hi) = hover_idx {
            if let Some(&v) = vals.get(hi) {
                let frac = (v / max_v.max(1e-6)).clamp(0.0, 1.0) as f32;
                let hx = if n > 1 { px + hi as f32 * step_x } else { px + pw * 0.5 };
                let hy = py + ph * (1.0 - frac);
                self.draw.rect(hx, py, 1.0, ph, [0.92, 0.91, 0.89, 0.35]);
                self.draw.circle(hx, hy, 3.0, color);
                if let Some(b) = buckets.get(hi) {
                    if self.is_hovered(px, py, pw, ph) {
                        let _ = origin;
                        self.hover_target = Some(HoverTarget::HeapPoint {
                            ts_ms: b.ts_ms,
                            retained: b.retained_bytes,
                            alloc_bytes: b.alloc_bytes,
                            slope: self.world.heap_slope_bps(),
                        });
                    }
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
        self.draw.text(r.x + 26.0, r.y + 7.5, 10.0, col::INK3, "STALLS");

        let cur = hover_idx.and_then(|i| buckets.get(i)).or_else(|| buckets.last());
        if let Some(b) = cur {
            let compact = r.w < 250.0;
            let io_s = if compact {
                format!("IO {}", fmt_ns(b.io_ns))
            } else {
                format!("I/O {}", fmt_ns(b.io_ns))
            };
            let lk_s = if compact {
                format!("L {}", fmt_ns(b.lock_ns))
            } else {
                format!("Lock {}", fmt_ns(b.lock_ns))
            };
            let wio = self.draw.measure_text(&io_s, 9.2);
            let wlk = self.draw.measure_text(&lk_s, 9.2);
            let io_x = r.x + r.w - wio - 8.0;
            let lk_x = io_x - wlk - 10.0;
            if io_x > r.x + 74.0 {
                self.draw.text(io_x, r.y + 8.0, 9.2, col::WARN, &io_s);
            }
            if lk_x > r.x + 74.0 {
                self.draw.text(lk_x, r.y + 8.0, 9.2, col::DANGER, &lk_s);
            }
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
                let _ = origin;
                if self.is_hovered(px, py, pw, ph) {
                    self.hover_target = Some(HoverTarget::StallPoint {
                        ts_ms: b.ts_ms,
                        lock_ns: b.lock_ns,
                        io_ns: b.io_ns,
                    });
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
            if is_hov {
                self.hover_target = Some(HoverTarget::MapCard(f.slot));
            }

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
            if self.is_hovered(cx - 4.0, cy, col_w - 8.0, row_h) {
                self.draw
                    .rrect(cx - 4.0, cy, col_w - 8.0, row_h - 2.0, 4.0, [1.0, 1.0, 1.0, 0.04]);
                self.hover_target = Some(HoverTarget::Core(c));
            }

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
            let is_hov = self.is_hovered(x0, ry, w, row_h);
            if is_sel {
                self.draw.rect(x0 + 1.0, ry + 1.0, w - 2.0, row_h - 1.0, col::FIRE_SOFT);
            } else if is_hov {
                self.draw.rect(x0 + 1.0, ry + 1.0, w - 2.0, row_h - 1.0, [1.0, 1.0, 1.0, 0.035]);
            }
            if is_hov {
                self.hover_target = Some(HoverTarget::MapCard(f.slot));
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
        let sites = self.world.heap_sites_for_window(self.heap_window);

        self.draw_btn(area.x + pad, sub_y, 108.0, 22.0, "Take Snapshot", false, Action::HeapTakeSnap);
        let mut lx = area.x + pad + 116.0;
        if self.world.heap_baseline.is_some() {
            self.draw_btn(lx, sub_y, 102.0, 22.0, "Clear Baseline", false, Action::HeapClearSnap);
            lx += 110.0;
        }
        if let Some((w0, w1)) = self.heap_window {
            let w_lbl = format!("Window {}  ×", fmt_span((w1 - w0).max(1.0)));
            let ww = self.draw.measure_text(&w_lbl, 10.0) + 16.0;
            let hov = self.is_hovered(lx, sub_y, ww, 22.0);
            self.draw.card(
                lx,
                sub_y,
                ww,
                22.0,
                5.0,
                if hov { col::SERIES2 } else { col::SERIES2_SOFT },
                col::SERIES2,
            );
            self.draw.text(
                lx + 8.0,
                sub_y + 4.5,
                10.0,
                if hov { col::WHITE } else { col::INK1 },
                &w_lbl,
            );
            self.push_hit(lx, sub_y, ww, 22.0, Action::ClearHeapWindow, "pointer");
            lx += ww + 10.0;
        }
        let win_ret: u64 = sites.iter().map(|s| s.retained_bytes).sum();
        let win_tot: u64 = sites.iter().map(|s| s.total_bytes).sum();
        let win_freed = win_tot.saturating_sub(win_ret);
        let win_live_objs: u64 = sites.iter().map(|s| s.allocs.saturating_sub(s.frees)).sum();
        let leak_suspects = sites
            .iter()
            .filter(|s| {
                let surv = if s.allocs > 0 {
                    s.allocs.saturating_sub(s.frees) as f64 / s.allocs as f64
                } else {
                    0.0
                };
                s.allocs >= 4 && surv >= 0.85 && s.retained_bytes > 0
            })
            .count();

        let status = if let Some((w0, w1)) = self.heap_window {
            let origin = self.world.trace_origin_ms.unwrap_or(w0);
            format!(
                "+{}..+{} · {} net · {} alloc · {} stacks",
                fmt_span((w0 - origin).max(0.0)),
                fmt_span((w1 - origin).max(0.0)),
                fmt_bytes(win_ret as i64),
                fmt_bytes(win_tot as i64),
                sites.len()
            )
        } else if self.world.heap_baseline.is_some() {
            format!("Diff vs snapshot ({})", self.world.heap_baseline_label)
        } else {
            format!(
                "{} retained · {} live objs · Drag HEAP chart above to slice a window",
                fmt_bytes(self.world.total_retained_bytes() as i64),
                self.world.live_ptrs.len()
            )
        };
        let status_max = (area.x + area.w - lx - pad).max(40.0);
        self.draw
            .text_max(lx, sub_y + 4.5, 10.0, col::INK3, &status, status_max);

        let x0 = area.x + pad;
        let w = (area.w - pad * 2.0 - 6.0).max(280.0);
        let scroll = self.scroll_of(PaneId::HeapDiff);
        let body_top = sub_y + 28.0;
        let body_h = (area.y + area.h - body_top).max(20.0);
        let mut y0 = body_top + 2.0 - scroll;

        // Clip scrolled content cleanly below the fixed Take Snapshot / Window subbar!
        self.draw.push_clip(area.x, body_top, area.w, body_h);

        // Visual Heap Composition Strip (segmented site breakdown + human KPIs)
        let palette = [col::FIRE, col::SERIES2, col::OK, col::WARN, col::SERIES4];
        let strip_h = 44.0;
        self.draw.card(x0, y0, w, strip_h, 7.0, col::BG2, col::LINE);
        let mut kx = x0 + 10.0;
        let k_ret = format!("RETAINED {}", fmt_bytes(win_ret as i64));
        kx += self.draw.text(kx, y0 + 7.0, 9.8, col::INK1, &k_ret) + 14.0;
        let k_ch = format!("FREED {}", fmt_bytes(win_freed as i64));
        kx += self.draw.text(kx, y0 + 7.0, 9.8, col::INK3, &k_ch) + 14.0;
        let k_live = format!("{win_live_objs} live allocs");
        kx += self.draw.text(kx, y0 + 7.0, 9.8, col::INK3, &k_live) + 14.0;
        if leak_suspects > 0 {
            let k_lk = format!(
                "{leak_suspects} leak suspect{}",
                if leak_suspects == 1 { "" } else { "s" }
            );
            self.draw.text(kx, y0 + 7.0, 9.8, col::DANGER, &k_lk);
        } else if !sites.is_empty() {
            self.draw.text(kx, y0 + 7.0, 9.8, col::OK, "No leaks detected");
        }

        let bar_x = x0 + 10.0;
        let bar_y = y0 + 25.0;
        let bar_w = (w - 20.0).max(40.0);
        self.draw.rrect(bar_x, bar_y, bar_w, 10.0, 4.0, col::BG4);
        let total_ret = win_ret.max(1);
        let mut sx = bar_x;
        for (i, s) in sites.iter().enumerate() {
            if s.retained_bytes == 0 {
                continue;
            }
            let frac = (s.retained_bytes as f64 / total_ret as f64) as f32;
            let sw = (bar_w * frac).max(4.0).min(bar_x + bar_w - sx);
            if sw <= 0.5 {
                break;
            }
            let seg_col = palette[i % palette.len()];
            self.draw.rrect(sx, bar_y, (sw - 1.5).max(2.5), 10.0, 3.0, seg_col);
            if self.is_hovered(sx, bar_y - 2.0, sw, 14.0) {
                self.draw
                    .border(sx, bar_y, (sw - 1.5).max(2.5), 10.0, 3.0, 1.0, col::WHITE);
                self.hover_target = Some(HoverTarget::HeapSite(s.clone()));
            }
            if let Some(slot) = s.slot {
                self.push_hit(sx, bar_y, sw, 10.0, Action::InspectSlot(slot), "pointer");
            }
            sx += sw;
        }
        y0 += strip_h + 8.0;

        // Diagnostic Allocation Sites Table
        let head_h = 24.0;
        let row_h = 38.0;
        let table_h = head_h + (sites.len().max(1) as f32) * row_h;
        self.draw.card(x0, y0, w, table_h, 7.0, col::BG2, col::LINE);
        self.draw.rect(x0 + 1.0, y0 + 1.0, w - 2.0, head_h, [1.0, 1.0, 1.0, 0.03]);
        self.draw.rect(x0, y0 + head_h, w, 1.0, col::LINE);

        let cx = [
            x0 + 10.0,
            x0 + w * 0.36,
            x0 + w * 0.45,
            x0 + w * 0.59,
            x0 + w * 0.68,
            x0 + w * 0.79,
            x0 + w * 0.90,
        ];
        for (i, h) in [
            "ALLOCATION SITE",
            "STATUS",
            "SHARE",
            "AVG SIZE",
            "ALLOC / FREE",
            "RETAINED",
            "Δ SNAP",
        ]
        .iter()
        .enumerate()
        {
            self.draw.text(cx[i], y0 + 6.0, 9.5, col::INK3, h);
        }

        if sites.is_empty() {
            let empty_msg = if self.heap_window.is_some() {
                "No allocations recorded inside this dragged window"
            } else {
                "No heap allocations recorded yet"
            };
            self.draw
                .text(x0 + 10.0, y0 + head_h + 11.0, 10.5, col::INK3, empty_msg);
            self.draw.pop_clip();
            self.pane_max_scroll.insert(PaneId::HeapDiff, 0.0);
            return;
        }

        for (ri, s) in sites.iter().enumerate() {
            let ry = y0 + head_h + ri as f32 * row_h;
            if ri > 0 {
                self.draw.rect(x0, ry, w, 1.0, col::LINE);
            }
            let hov = self.is_hovered(x0, ry, w, row_h);
            if hov {
                self.draw
                    .rect(x0 + 1.0, ry + 1.0, w - 2.0, row_h - 1.0, [1.0, 1.0, 1.0, 0.04]);
                self.hover_target = Some(HoverTarget::HeapSite(s.clone()));
            }
            if let Some(slot) = s.slot {
                self.push_hit(x0, ry, w, row_h, Action::InspectSlot(slot), "pointer");
            }

            let dot_col = palette[ri % palette.len()];
            self.draw.circle(cx[0] + 4.0, ry + 12.5, 3.0, dot_col);

            // Line 1: Bold primary allocating fn + subtle immediate caller (no repetitive 3-frame text wall!)
            let site_max = (cx[1] - cx[0] - 20.0).max(60.0);
            let primary_fn = if s.top_fn.is_empty() { s.key.as_str() } else { s.top_fn.as_str() };
            let pw_fn = self
                .draw
                .text_max(cx[0] + 13.0, ry + 6.0, 10.4, col::INK1, primary_fn, site_max);
            if let Some(caller) = s.display_stack.split(" → ").nth(1) {
                let rem = site_max - pw_fn - 6.0;
                if rem >= 54.0 {
                    self.draw.text_max(
                        cx[0] + 13.0 + pw_fn + 6.0,
                        ry + 6.5,
                        9.4,
                        col::INK4,
                        &format!("← {caller}"),
                        rem,
                    );
                }
            }

            // Line 2: Compact basename `file.rs:line` + tiny thread pill `[t#N]`
            let mut mx = cx[0] + 13.0;
            if !s.file.is_empty() && s.line > 0 {
                let loc = format!("{}:{}", file_base(&s.file), s.line);
                mx += self
                    .draw
                    .text_max(mx, ry + 21.5, 9.2, col::INK3, &loc, (site_max - 42.0).max(40.0))
                    + 8.0;
            }
            if s.thread > 0 {
                self.draw.badge(
                    mx,
                    ry + 19.5,
                    14.5,
                    8.5,
                    &format!("t#{}", s.thread),
                    col::INK3,
                    col::BG3,
                    col::LINE,
                );
            }

            // Status diagnostic badge (`LEAK` / `GROWING` / `CHURN` / `OK`)
            let live_cnt = s.allocs.saturating_sub(s.frees);
            let survival = if s.allocs > 0 {
                live_cnt as f64 / s.allocs as f64
            } else {
                0.0
            };
            let (st_lbl, st_fg, st_bg) = if s.allocs >= 4 && survival >= 0.85 && s.retained_bytes > 0 {
                ("LEAK", col::DANGER, col::DANGER_SOFT)
            } else if s.retained_bytes > 0 && survival >= 0.45 {
                ("GROWING", col::WARN, [0.85, 0.58, 0.19, 0.16])
            } else if s.allocs >= 6 && s.retained_bytes == 0 {
                ("CHURN", col::SERIES2, col::SERIES2_SOFT)
            } else {
                ("OK", col::OK, col::OK_SOFT)
            };
            self.draw.badge(
                cx[1],
                ry + 10.0,
                17.0,
                8.8,
                st_lbl,
                st_fg,
                st_bg,
                col::with_alpha(st_fg, 0.45),
            );

            // Proportional Share bar + %
            let share = (s.retained_bytes as f64 / total_ret as f64).clamp(0.0, 1.0) as f32;
            let pct_lbl = format!("{:.0}%", share * 100.0);
            let sb_w = (cx[3] - cx[2] - 40.0).max(20.0);
            self.draw.rrect(cx[2], ry + 16.5, sb_w, 5.0, 2.5, col::BG4);
            if s.retained_bytes > 0 {
                self.draw.rrect(
                    cx[2],
                    ry + 16.5,
                    (sb_w * share).clamp(3.0, sb_w),
                    5.0,
                    2.5,
                    dot_col,
                );
            }
            self.draw
                .text(cx[2] + sb_w + 6.0, ry + 12.5, 9.5, col::INK3, &pct_lbl);

            // Avg allocation size (+ range if variable)
            let avg = if s.allocs > 0 {
                s.total_bytes / s.allocs
            } else {
                0
            };
            self.draw
                .text(cx[3], ry + 7.0, 10.0, col::INK1, &fmt_bytes(avg as i64));
            let range_s = if s.min_bytes < s.max_bytes && s.min_bytes < u64::MAX {
                format!("{}..{}", fmt_bytes(s.min_bytes as i64), fmt_bytes(s.max_bytes as i64))
            } else {
                "fixed".into()
            };
            self.draw.text_max(
                cx[3],
                ry + 21.0,
                9.0,
                col::INK3,
                &range_s,
                (cx[4] - cx[3] - 8.0).max(24.0),
            );

            // Alloc / Free + visual inline survival meter (visual primitive instead of noisy text!)
            self.draw.text(
                cx[4],
                ry + 7.0,
                10.0,
                col::INK2,
                &format!("{} / {}", s.allocs, s.frees),
            );
            let meter_w = (cx[5] - cx[4] - 46.0).clamp(26.0, 56.0);
            self.draw.rrect(cx[4], ry + 23.5, meter_w, 4.0, 2.0, col::BG4);
            let surv_col = if survival >= 0.85 && s.allocs >= 4 {
                col::DANGER
            } else if survival >= 0.45 {
                col::WARN
            } else {
                col::OK
            };
            if survival > 0.01 {
                self.draw.rrect(
                    cx[4],
                    ry + 23.5,
                    (meter_w * survival as f32).clamp(3.0, meter_w),
                    4.0,
                    2.0,
                    surv_col,
                );
            }
            self.draw.text(
                cx[4] + meter_w + 6.0,
                ry + 20.0,
                8.8,
                surv_col,
                &format!("{:.0}%", survival * 100.0),
            );

            // Retained net + total allocated
            self.draw.text(
                cx[5],
                ry + 7.0,
                10.2,
                if s.retained_bytes > 0 { col::INK1 } else { col::INK3 },
                &fmt_bytes(s.retained_bytes as i64),
            );
            self.draw.text(
                cx[5],
                ry + 21.0,
                9.0,
                col::INK3,
                &format!("{} tot", fmt_bytes(s.total_bytes as i64)),
            );

            // Δ Snapshot
            if let Some(ref snap) = self.world.heap_baseline {
                let prev = snap.get(&s.key).copied().unwrap_or(0) as i64;
                let diff = s.retained_bytes as i64 - prev;
                self.draw.text(
                    cx[6],
                    ry + 12.5,
                    10.0,
                    if diff > 0 {
                        col::FIRE
                    } else if diff < 0 {
                        col::OK
                    } else {
                        col::INK3
                    },
                    &format!("{}{}", if diff >= 0 { "+" } else { "" }, fmt_bytes(diff)),
                );
            } else {
                self.draw.text(cx[6], ry + 12.5, 10.0, col::INK4, "—");
            }
        }
        self.draw.pop_clip();
        let content_h = (y0 + scroll - body_top) + table_h + 16.0;
        self.draw_vscrollbar(PaneId::HeapDiff, area.x + area.w - 9.0, body_top, body_h, content_h);
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
            if self.is_hovered(x0, ry, w, row_h) {
                self.draw
                    .rect(x0 + 1.0, ry + 1.0, w - 2.0, row_h - 1.0, [1.0, 1.0, 1.0, 0.04]);
                self.hover_target = Some(HoverTarget::Anomaly(ins.clone()));
            }
            let (fg, bg) = match ins.severity {
                2 => (col::DANGER, col::DANGER_SOFT),
                1 => (col::FIRE, col::FIRE_SOFT),
                _ => (col::OK, col::OK_SOFT),
            };
            let ico = insight_icon(ins.category);
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
            let act = insight_action(ins);
            self.draw_btn(x0 + w - 32.0, ry + 6.0, 24.0, 22.0, "→", false, act);
        }
        self.draw_vscrollbar(PaneId::Advisor, area.x + area.w - 9.0, area.y, area.h, box_h + 24.0);
    }

    // -----------------------------------------------------------------------
    // Pane: I/O & Locks
    // -----------------------------------------------------------------------

    fn render_io_pane(&mut self, area: Rect) {
        let pad = 10.0;
        let x0 = area.x + pad;
        let w = (area.w - pad * 2.0 - 6.0).max(280.0);
        let scroll = self.scroll_of(PaneId::IoLocks);
        let mut y0 = area.y + 8.0 - scroll;

        // Open Ports & Active Sockets card (when process exposes sockets/FDs)
        let sockets = self.world.resources.sockets.clone();
        if !sockets.is_empty() || self.world.resources.fd_count > 0 {
            let head_h = 24.0;
            let row_h = 24.0;
            let s_rows = sockets.len().min(12);
            let card_h = head_h + (s_rows.max(1) as f32) * row_h;
            self.draw.card(x0, y0, w, card_h, 7.0, col::BG2, col::LINE);
            self.draw.rect(x0 + 1.0, y0 + 1.0, w - 2.0, head_h, [1.0, 1.0, 1.0, 0.03]);
            self.draw.rect(x0, y0 + head_h, w, 1.0, col::LINE);
            self.draw.text(x0 + 10.0, y0 + 6.0, 9.5, col::INK3, "PROTO");
            self.draw.text(x0 + w * 0.14, y0 + 6.0, 9.5, col::INK3, "STATE");
            self.draw.text(x0 + w * 0.30, y0 + 6.0, 9.5, col::INK3, "LOCAL");
            self.draw.text(x0 + w * 0.62, y0 + 6.0, 9.5, col::INK3, "PEER");
            let fd_s = format!("{} FDs", self.world.resources.fd_count);
            let fdw = self.draw.measure_text(&fd_s, 9.5);
            self.draw.text(x0 + w - fdw - 10.0, y0 + 6.0, 9.5, col::INK3, &fd_s);

            if s_rows == 0 {
                self.draw
                    .text(x0 + 10.0, y0 + head_h + 6.0, 10.0, col::INK3, "No open IP sockets");
            } else {
                for (si, s) in sockets.iter().take(12).enumerate() {
                    let ry = y0 + head_h + si as f32 * row_h;
                    if si > 0 {
                        self.draw.rect(x0, ry, w, 1.0, col::LINE);
                    }
                    let is_listen = s.state.eq_ignore_ascii_case("LISTEN");
                    self.draw.text(x0 + 10.0, ry + 5.5, 10.0, col::INK1, &s.proto);
                    self.draw.text(
                        x0 + w * 0.14,
                        ry + 5.5,
                        10.0,
                        if is_listen { col::OK } else { col::SERIES2 },
                        &s.state,
                    );
                    self.draw.text_max(x0 + w * 0.30, ry + 5.5, 10.0, col::INK1, &s.local, w * 0.30);
                    self.draw.text_max(
                        x0 + w * 0.62,
                        ry + 5.5,
                        10.0,
                        col::INK3,
                        if s.remote.is_empty() { "—" } else { &s.remote },
                        w * 0.35,
                    );
                    if is_listen && s.port > 0 {
                        self.push_hit(
                            x0,
                            ry,
                            w,
                            row_h,
                            Action::OpenUrl(format!("http://127.0.0.1:{}", s.port)),
                            "pointer",
                        );
                    }
                }
            }
            y0 += card_h + 10.0;
        }

        let rows: Vec<_> = self.world.io_rows.iter().rev().take(80).cloned().collect();
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
        } else {
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
                    format!(
                        "holder t#{} in {} · mutex 0x{:x}",
                        r.holder_thread,
                        self.world.pc_label(r.holder_pc),
                        r.arg
                    )
                } else {
                    format!("fd={} · bytes={}", r.arg, r.res)
                };
                self.draw.text_max(cx[5], ty, 10.0, col::INK2, &attr, x0 + w - cx[5] - 10.0);
                if is_lock {
                    if let Some(slot) = self.world.resolve_pc(r.holder_pc).and_then(|s| s.slot) {
                        self.push_hit(x0, ry, w, row_h, Action::InspectSlot(slot), "pointer");
                    }
                }
            }
        }
        let content_h = (y0 + scroll - area.y) + table_h + 18.0;
        self.draw_vscrollbar(PaneId::IoLocks, area.x + area.w - 9.0, area.y, area.h, content_h);
    }

    // -----------------------------------------------------------------------
    // Pane: Modular Read-Only Post-IDE Inspector (Dockable Anywhere + Expandable!)
    // -----------------------------------------------------------------------

    fn current_code_loc(&self) -> Option<(String, u32)> {
        if let Some(ref cv) = self.code_view {
            return Some(cv.clone());
        }
        let slot = self.inspected_slot.or_else(|| {
            self.world
                .fns
                .iter()
                .max_by_key(|(_, f)| f.total_ns)
                .map(|(_, f)| f.slot)
        })?;
        let fe = self.world.slot_entity.get(&slot)?;
        let f = self.world.fns.get(*fe)?;
        if !f.file.is_empty() && f.line > 0 {
            Some((f.file.clone(), f.line))
        } else {
            None
        }
    }

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
                "Click any function, allocation site, or span to inspect code",
            );
            return;
        };

        let Some(&fe) = self.world.slot_entity.get(&slot) else {
            return;
        };
        let Some(f) = self.world.fns.get(fe).cloned() else {
            return;
        };

        let (cur_file, cur_line) = self
            .code_view
            .clone()
            .unwrap_or_else(|| (f.file.clone(), f.line));
        if !cur_file.is_empty() && cur_line > 0 {
            self.queue_source_if_needed(&cur_file, cur_line);
        }

        let pad = 14.0;
        let scroll = self.scroll_of(PaneId::Inspector);
        let mut y = area.y + 12.0 - scroll;

        // Top header: optional `← Back` button when jumped via Go-to-Definition + function name + `Open ↗`
        let mut hx_left = area.x + pad;
        if !self.code_history.is_empty() {
            self.draw_btn(hx_left, y - 3.0, 56.0, 22.0, "← Back", false, Action::SourceBack);
            hx_left += 64.0;
        }
        let header_title = if self.code_view.is_some() {
            format!("{}:{}", file_base(&cur_file), cur_line)
        } else {
            f.name.clone()
        };
        self.draw.text_max(
            hx_left,
            y,
            12.2,
            col::INK1,
            &header_title,
            (area.x + area.w - hx_left - 72.0).max(60.0),
        );
        if !cur_file.is_empty() && cur_line > 0 {
            self.draw_btn(
                area.x + area.w - 66.0,
                y - 3.0,
                52.0,
                22.0,
                "Open ↗",
                false,
                Action::OpenEditor(cur_file.clone(), cur_line),
            );
        }
        y += 18.0;

        let loc_str = if !cur_file.is_empty() {
            format!("{}:{}", short_path(&cur_file), cur_line)
        } else {
            format!("slot #{slot}")
        };
        self.draw
            .text_max(area.x + pad, y, 9.8, col::INK3, &loc_str, area.w - pad * 2.0);
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

        // Full-file virtualized Post-IDE Rust Source Viewer (hover symbols, click Go-to-Definition, zero random cutoffs!)
        let full_lines_opt = self.world.file_lines.get(&cur_file).cloned();
        let line_count = full_lines_opt.as_ref().map(|v| v.len()).unwrap_or(0);
        let src_hdr = if line_count > 0 {
            format!("SOURCE · {} ({} lines)", file_base(&cur_file), line_count)
        } else {
            "SOURCE".into()
        };
        self.draw
            .text_max(area.x + pad, y, 9.5, col::INK3, &src_hdr, (area.w - pad * 2.0).max(60.0));
        y += 15.0;

        let line_h = 17.0_f32;
        let unscrolled_y = (y + scroll) - area.y;
        let box_h = (area.h - unscrolled_y - 12.0).max(260.0);
        let box_w = (area.w - pad * 2.0 - 6.0).max(120.0);
        let code_gutter = 42.0;
        let vbar_w = 10.0;
        let code_view_w = (box_w - code_gutter - vbar_w - 6.0).max(40.0);

        let total_code_h = (line_count as f32) * line_h + 18.0;
        let inner_view_h = (box_h - 14.0).max(40.0);
        self.source_max_scroll_y = (total_code_h - inner_view_h).max(0.0);

        // Auto-center `cur_line` (4 lines from top of SOURCE box) whenever inspecting a new slot or jumping!
        if let Some((ref af, al)) = self.source_auto_center {
            if af == &cur_file && line_count > 0 {
                let target_y = (al.saturating_sub(5) as f32) * line_h;
                self.source_scroll_y = target_y.clamp(0.0, self.source_max_scroll_y);
                self.source_auto_center = None;
            }
        } else if line_count > 0 && self.source_scroll_y == 0.0 && cur_line > 8 {
            let target_y = (cur_line.saturating_sub(5) as f32) * line_h;
            self.source_scroll_y = target_y.clamp(0.0, self.source_max_scroll_y);
        }
        self.source_scroll_y = self.source_scroll_y.clamp(0.0, self.source_max_scroll_y);

        // Find active heap allocation site for this slot (to render an inline inlay badge on the target line!)
        let slot_heap_badge: Option<(String, col::Rgba)> = if self.code_view.is_none() {
            self.world
                .heap_sites
                .iter()
                .map(|(_, s)| s)
                .find(|s| s.slot == Some(slot) && s.allocs > 0)
                .map(|s| {
                    let avg = s.total_bytes / s.allocs.max(1);
                    let live = s.allocs.saturating_sub(s.frees);
                    let surv = live as f64 / s.allocs.max(1) as f64;
                    if s.allocs >= 4 && surv >= 0.85 && s.retained_bytes > 0 {
                        (format!("LEAK · {}/alloc · {}", fmt_bytes(avg as i64), fmt_bytes(s.retained_bytes as i64)), col::DANGER)
                    } else if s.retained_bytes > 0 {
                        (format!("{} retained · {}/alloc", fmt_bytes(s.retained_bytes as i64), fmt_bytes(avg as i64)), col::WARN)
                    } else {
                        (format!("CHURN · {} tot", fmt_bytes(s.total_bytes as i64)), col::SERIES2)
                    }
                })
        } else {
            None
        };

        // Measure max visible line width for horizontal scrollbar (fast: only visible window + margin!)
        let first_vis = ((self.source_scroll_y / line_h).floor() as usize).min(line_count.saturating_sub(1));
        let vis_count = ((inner_view_h / line_h).ceil() as usize + 3).min(line_count.saturating_sub(first_vis));
        let max_line_w = if let Some(ref fl) = full_lines_opt {
            fl.iter()
                .skip(first_vis)
                .take(vis_count)
                .map(|t| self.draw.measure_text(t, 10.2) + 120.0)
                .fold(0.0_f32, f32::max)
        } else {
            0.0
        };
        self.source_max_scroll_x = (max_line_w - code_view_w).max(0.0);
        self.source_scroll_x = self.source_scroll_x.clamp(0.0, self.source_max_scroll_x);
        let has_hscroll = self.source_max_scroll_x > 2.0;

        self.source_box_rect = Rect {
            x: area.x + pad,
            y,
            w: box_w,
            h: box_h,
        };
        self.draw
            .card(area.x + pad, y, box_w, box_h, 7.0, [0.055, 0.055, 0.060, 1.0], col::LINE);

        let code_clip_h = box_h - if has_hscroll { 14.0 } else { 2.0 };
        self.draw
            .push_clip(area.x + pad + 1.0, y + 1.0, box_w - 2.0, code_clip_h);

        let mut pending_def_req: Option<(String, crate::rust_ide::CrossFileQuery)> = None;
        if let Some(ref full_lines) = full_lines_opt {
            let graph = self
                .world
                .file_graphs
                .get(&cur_file)
                .cloned()
                .unwrap_or_default();
            let adv = 10.2 * self.draw.advance_ratio;
            let code_x0 = area.x + pad + code_gutter + 2.0;

            for idx in first_vis..(first_vis + vis_count).min(full_lines.len()) {
                let n = (idx + 1) as u32;
                let text = &full_lines[idx];
                let ly = y + 7.0 + (idx as f32) * line_h - self.source_scroll_y;
                if ly < y + 1.0 || ly + line_h > y + code_clip_h {
                    continue;
                }
                let is_t = n == cur_line;
                if is_t {
                    self.draw
                        .rect(area.x + pad + 1.0, ly - 1.0, box_w - 2.0 - vbar_w, line_h, col::FIRE_SOFT);
                }
                // Sticky line numbers
                self.draw.text(
                    area.x + pad + 6.0,
                    ly + 1.5,
                    9.6,
                    if is_t { col::FIRE } else { col::INK4 },
                    &format!("{n:>4}"),
                );

                // Interactive identifier hover & Go-to-Definition inside the Rust code line!
                let line_hovered = self.mouse_inside
                    && self.mouse_x >= code_x0
                    && self.mouse_x <= code_x0 + code_view_w
                    && self.mouse_y >= ly.max(y)
                    && self.mouse_y < (ly + line_h).min(y + code_clip_h);

                self.draw
                    .push_clip(area.x + pad + code_gutter, ly - 1.0, code_view_w, line_h + 2.0);
                let draw_x = code_x0 - self.source_scroll_x;
                self.draw
                    .rust_code_line(draw_x, ly + 1.5, 10.2, max_line_w + 60.0, text);

                // Inline inlay hints: heap allocation calls or target-line perf/leak summary!
                let line_px = self.draw.measure_text(text, 10.2);
                let inlay_x = draw_x + line_px + 12.0;
                if is_t {
                    if let Some((ref badge_s, bcol)) = slot_heap_badge {
                        self.draw.badge(
                            inlay_x,
                            ly + 0.5,
                            14.5,
                            8.4,
                            badge_s,
                            bcol,
                            col::with_alpha(bcol, 0.14),
                            col::with_alpha(bcol, 0.4),
                        );
                    }
                } else if let Some(alloc_tag) = crate::rust_ide::find_alloc_hint(text) {
                    self.draw.badge(
                        inlay_x,
                        ly + 0.5,
                        14.5,
                        8.4,
                        alloc_tag,
                        col::SERIES2,
                        col::SERIES2_SOFT,
                        col::with_alpha(col::SERIES2, 0.35),
                    );
                }

                // Token-aware, scope-graph-backed semantic hover & Go-to-Definition!
                if line_hovered {
                    let rel_x = (self.mouse_x - draw_x).max(0.0);
                    let char_col = (rel_x / adv.max(1.0)).floor() as usize;
                    if let Some(hit) = crate::rust_ide::ident_at_col(full_lines, idx, char_col) {
                        let resolved = match crate::rust_ide::resolve_hit(&cur_file, full_lines, &graph, n, &hit) {
                            crate::rust_ide::ResolveOutcome::Resolved(mut sym) => {
                                // Enrich param/local target_type (e.g. `event: Event`) from scoped crate graph
                                if sym.snippet.is_empty() {
                                    if let Some(ref tt) = sym.target_type.clone() {
                                        let imp = graph.imports.get(tt);
                                        let q = crate::rust_ide::CrossFileQuery {
                                            ident: imp.map(|x| x.orig_name.clone()).unwrap_or_else(|| tt.clone()),
                                            prefix: None,
                                            import_path: imp.map(|x| x.path.clone()),
                                            want_kind: "type",
                                        };
                                        let ck = Self::def_cache_key(&cur_file, &q);
                                        if let Some(Some(ws_def)) = self.world.workspace_defs.get(&ck) {
                                            sym.snippet = ws_def.snippet.clone();
                                            if sym.doc.is_empty() {
                                                sym.doc = ws_def.doc.clone();
                                            }
                                            sym.file = ws_def.file.clone();
                                            sym.line = ws_def.line;
                                        } else if !self.world.workspace_defs.contains_key(&ck) {
                                            pending_def_req = Some((cur_file.clone(), q));
                                        }
                                    }
                                }
                                Some(sym)
                            }
                            crate::rust_ide::ResolveOutcome::NeedCrossFile(q) => {
                                let ck = Self::def_cache_key(&cur_file, &q);
                                if let Some(cached) = self.world.workspace_defs.get(&ck) {
                                    cached.clone()
                                } else {
                                    pending_def_req = Some((cur_file.clone(), q));
                                    None
                                }
                            }
                            crate::rust_ide::ResolveOutcome::None => None,
                        };

                        if let Some(sym) = resolved {
                            let ux0 = draw_x + (hit.col_start as f32) * adv;
                            let uw = ((hit.col_end - hit.col_start) as f32) * adv;
                            let can_jump = !sym.file.is_empty() && sym.line > 0 && !(sym.file == cur_file && sym.line == n);
                            let ucol = if can_jump { col::FIRE } else { col::SERIES2 };
                            self.draw.rect(ux0, ly + line_h - 2.5, uw, 1.3, ucol);
                            if can_jump {
                                self.push_hit(
                                    ux0.max(code_x0),
                                    ly,
                                    uw,
                                    line_h,
                                    Action::JumpSource(sym.file.clone(), sym.line),
                                    "pointer",
                                );
                            }
                            self.hover_target = Some(HoverTarget::CodeSym(sym));
                        }
                    }
                }
                self.draw.pop_clip();
            }
        } else {
            self.draw.text(
                area.x + pad + 12.0,
                y + 12.0,
                10.2,
                col::INK3,
                if cur_file.is_empty() { "No source path" } else { "Loading full file…" },
            );
        }
        if let Some((from, q)) = pending_def_req {
            self.queue_def_if_needed(&from, &q);
        }
        self.draw.pop_clip();

        // Dedicated vertical scrollbar inside SOURCE box (+ fire tick mark showing active target line!)
        if self.source_max_scroll_y > 1.0 && line_count > 0 {
            let track = Rect {
                x: area.x + pad + box_w - 9.0,
                y: y + 4.0,
                w: 6.0,
                h: (code_clip_h - 8.0).max(24.0),
            };
            let thumb_h = ((inner_view_h / total_code_h.max(1.0)) * track.h).clamp(24.0, track.h);
            let thumb_y = track.y + (self.source_scroll_y / self.source_max_scroll_y) * (track.h - thumb_h);
            let thumb = Rect {
                x: track.x,
                y: thumb_y,
                w: 6.0,
                h: thumb_h,
            };
            let dragging = matches!(self.drag_op, DragOp::VScrollSource { .. });
            let hov = self.is_hovered(track.x - 3.0, track.y, track.w + 6.0, track.h);
            self.draw
                .rrect(track.x, track.y, track.w, track.h, 3.0, [0.09, 0.09, 0.10, 0.9]);
            // Subtle target-line marker on scrollbar track
            let t_norm = ((cur_line.saturating_sub(1) as f32) / line_count.max(1) as f32).clamp(0.0, 1.0);
            self.draw
                .rect(track.x - 1.0, track.y + t_norm * track.h, track.w + 2.0, 2.0, col::FIRE);
            self.draw.rrect(
                thumb.x,
                thumb.y,
                thumb.w,
                thumb.h,
                3.0,
                if dragging {
                    col::FIRE
                } else if hov {
                    col::INK2
                } else {
                    [0.36, 0.36, 0.39, 0.85]
                },
            );
            self.vscroll_source = Some(VScrollHit {
                pane: PaneId::Inspector,
                track,
                thumb,
                max_scroll: self.source_max_scroll_y,
            });
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
        let icon;
        let mut icon_col = col::FIRE;
        let title: String;
        let mut loc_tag = String::new();
        let mut badge_txt = String::new();
        let mut badge_col = col::FIRE;
        let mut rust_sig: Option<String> = None;
        let mut stats: Vec<(&'static str, String, col::Rgba)> = Vec::new();
        let mut bars: Vec<(String, String, f32, col::Rgba)> = Vec::new();
        let mut chips: Vec<(String, col::Rgba)> = Vec::new();
        let mut chips_is_chain = false;
        let mut note: Option<String> = None;
        let mut code_key: Option<(String, u32)> = None;
        let mut inline_snippet: Option<(Vec<(u32, String)>, u32)> = None;

        match target {
            HoverTarget::Bar(bar) => match bar.kind {
                BarKind::Span => {
                    icon = Icon::Flame;
                    title = self.world.fn_name(bar.slot_or_op);
                    if let Some(&fe) = self.world.slot_entity.get(&bar.slot_or_op) {
                        if let Some(f) = self.world.fns.get(fe) {
                            let self_ns = f.total_ns.saturating_sub(f.child_ns);
                            stats.push(("DUR", fmt_ns(bar.dur_ns), col::INK1));
                            stats.push(("SELF", fmt_ns(self_ns), col::FIRE));
                            stats.push(("P95", fmt_ns(percentile(&f.samples, 0.95)), col::WARN));
                            stats.push(("CALLS", format!("{}×", f.calls), col::OK));
                            if !f.file.is_empty() && f.line > 0 {
                                loc_tag = format!("{}:{}", file_base(&f.file), f.line);
                                code_key = Some((f.file.clone(), f.line));
                            }
                        }
                    }
                    if bar.cpu != CPU_UNKNOWN {
                        let is_e = (bar.flags & 0x10) != 0;
                        badge_txt = format!("C{} · {}", bar.cpu, if is_e { "E" } else { "P" });
                        badge_col = if is_e { col::SERIES2 } else { col::OK };
                    }
                }
                BarKind::Lock => {
                    icon = Icon::Lock;
                    icon_col = col::DANGER;
                    title = "Mutex Stall".into();
                    badge_txt = fmt_ns(bar.dur_ns);
                    badge_col = col::DANGER;
                    stats.push(("WAITER", format!("t#{}", bar.thread), col::DANGER));
                    if bar.holder_thread > 0 {
                        stats.push(("HOLDER", format!("t#{}", bar.holder_thread), col::WARN));
                    }
                    stats.push(("MUTEX", format!("0x{:x}", bar.mutex_ptr), col::INK2));
                    chips.push((self.world.pc_label(bar.waiter_pc), col::DANGER));
                    if bar.holder_pc != 0 {
                        chips.push((self.world.pc_label(bar.holder_pc), col::WARN));
                        chips_is_chain = true;
                    }
                    if let Some(sym) = self.world.resolve_pc(bar.holder_pc) {
                        if !sym.file.is_empty() && sym.line > 0 {
                            loc_tag = format!("{}:{}", file_base(&sym.file), sym.line);
                            code_key = Some((sym.file.clone(), sym.line));
                        }
                    }
                }
                BarKind::Heap => {
                    icon = Icon::Memory;
                    icon_col = col::SERIES2;
                    title = self.world.pc_label(bar.holder_pc);
                    badge_txt = fmt_bytes(bar.bytes as i64);
                    badge_col = col::SERIES2;
                    stats.push(("SIZE", fmt_bytes(bar.bytes as i64), col::SERIES2));
                    stats.push(("THREAD", format!("t#{}", bar.thread), col::INK1));
                    stats.push(("PTR", format!("0x{:x}", bar.mutex_ptr), col::INK2));
                    if let Some(sym) = self.world.resolve_pc(bar.holder_pc) {
                        if !sym.file.is_empty() && sym.line > 0 {
                            loc_tag = format!("{}:{}", file_base(&sym.file), sym.line);
                            code_key = Some((sym.file.clone(), sym.line));
                        }
                    }
                }
                BarKind::Io => {
                    icon = Icon::Lock;
                    icon_col = col::WARN;
                    title = format!("I/O {}", io_op_name(bar.slot_or_op));
                    badge_txt = fmt_ns(bar.dur_ns);
                    badge_col = col::WARN;
                    stats.push(("WAIT", fmt_ns(bar.dur_ns), col::WARN));
                    stats.push(("THREAD", format!("t#{}", bar.thread), col::INK1));
                    stats.push(("FD", format!("#{}", bar.arg), col::INK2));
                }
            },
            HoverTarget::ThreadGutter(tid) => {
                icon = Icon::Cpu;
                icon_col = col::SERIES2;
                title = format!("Thread #{tid}");
                badge_col = col::SERIES2;
                if let Some(&te) = self.world.thread_entity.get(&tid) {
                    if let Some(tc) = self.world.threads.get(te) {
                        if tc.last_cpu != CPU_UNKNOWN {
                            let is_e = (tc.last_flags & 0x10) != 0;
                            badge_txt = format!("C{} · {}", tc.last_cpu, if is_e { "E" } else { "P" });
                            badge_col = if is_e { col::SERIES2 } else { col::OK };
                        }
                        stats.push((
                            "PARENT",
                            if tc.parent_tid > 0 { format!("t#{}", tc.parent_tid) } else { "main".into() },
                            col::INK1,
                        ));
                        stats.push((
                            "CORE HOPS",
                            tc.migrations.to_string(),
                            if tc.migrations >= 10 { col::FIRE } else { col::INK1 },
                        ));
                        chips_is_chain = true;
                        for &pc in &tc.spawn_frames {
                            if pc != 0 {
                                chips.push((self.world.pc_label(pc), col::INK2));
                            }
                        }
                    }
                }
            }
            HoverTarget::Flame(name, tot, slf, calls, root_tot) => {
                icon = Icon::Flame;
                title = name;
                let pct = (tot as f64 / root_tot.max(1) as f64) * 100.0;
                let spct = (slf as f64 / root_tot.max(1) as f64) * 100.0;
                badge_txt = format!("{pct:.1}%");
                badge_col = col::FIRE;
                stats.push(("TOTAL", fmt_ns(tot), col::INK1));
                stats.push(("SELF", fmt_ns(slf), col::FIRE));
                stats.push(("CALLS", format!("{calls}×"), col::OK));
                bars.push(("Self share".into(), format!("{spct:.1}%"), (spct as f32 / 100.0).clamp(0.0, 1.0), col::FIRE));
            }
            HoverTarget::Core(c) => {
                icon = Icon::Cpu;
                let top = &self.world.topology;
                let is_e = top.p_cores > 0 && (c as u32) >= top.p_cores;
                let live_pct = ((self.world.cpu_live_us[c] / 500_000.0) * 100.0).clamp(0.0, 100.0);
                let busy_us = self.world.cpu_busy_us[c];
                let evs = self.world.cpu_events[c];
                icon_col = if is_e { col::SERIES2 } else { col::FIRE };
                title = format!("Core {:02}", c);
                badge_txt = if is_e { "E-CORE".into() } else { "P-CORE".into() };
                badge_col = if is_e {
                    if live_pct > 35.0 { col::WARN } else { col::SERIES2 }
                } else {
                    col::FIRE
                };
                stats.push(("LOAD", format!("{live_pct:.0}%"), badge_col));
                stats.push(("CPU TIME", fmt_ns(busy_us * 1_000), col::INK1));
                stats.push(("EVENTS", evs.to_string(), col::INK2));
                bars.push(("Utilization".into(), format!("{live_pct:.0}%"), (live_pct / 100.0).clamp(0.0, 1.0), badge_col));
                let mut active: Vec<(u32, u64)> = self
                    .world
                    .threads
                    .iter()
                    .filter(|(_, t)| t.last_cpu as usize == c)
                    .map(|(_, t)| (t.tid, t.migrations))
                    .collect();
                active.sort_by_key(|(tid, _)| *tid);
                for (tid, hops) in active {
                    chips.push((format!("t#{tid} · {hops} hops"), if hops >= 10 { col::FIRE } else { col::INK2 }));
                }
                note = Some(if is_e {
                    "Efficiency core: lower clock & smaller L2 cache.".into()
                } else {
                    "Performance core: full clock & private L1d cache.".into()
                });
            }
            HoverTarget::Anomaly(ins) => {
                icon = insight_icon(ins.category);
                badge_col = match ins.severity {
                    2 => col::DANGER,
                    1 => col::FIRE,
                    _ => col::OK,
                };
                icon_col = badge_col;
                title = ins.title.clone();
                badge_txt = ins.metric.clone();
                match ins.category {
                    "CPU" => {
                        stats.push(("TOTAL HOPS", self.world.total_migrations.to_string(), col::DANGER));
                        stats.push(("THREADS", self.world.threads.len().to_string(), col::INK1));
                        note = Some("Core hops flush private L1/L2 caches and stall pipelines.".into());
                    }
                    "ECORE" => {
                        stats.push(("CLUSTER", "Efficiency".into(), col::WARN));
                        stats.push(("P-CORES", format!("{} avail", self.world.topology.p_cores), col::OK));
                        note = Some("Hot thread scheduled on low-clock E-core; raise QoS.".into());
                    }
                    "HEAP" => {
                        let sites = self.world.heap_sites_for_window(None);
                        let tot_ret = self.world.total_retained_bytes().max(1);
                        stats.push(("RETAINED", fmt_bytes(tot_ret as i64), col::DANGER));
                        stats.push(("LIVE OBJS", self.world.live_ptrs.len().to_string(), col::INK1));
                        let pal = [col::FIRE, col::SERIES2, col::OK];
                        for (i, s) in sites.iter().take(3).enumerate() {
                            let frac = (s.retained_bytes as f64 / tot_ret as f64).clamp(0.0, 1.0) as f32;
                            let lbl = if s.top_fn.is_empty() { s.key.clone() } else { s.top_fn.clone() };
                            bars.push((lbl, fmt_bytes(s.retained_bytes as i64), frac, pal[i % pal.len()]));
                        }
                    }
                    "LOCK" => {
                        let cnt = self.world.io_rows.iter().filter(|r| r.kind == "sync_wait").count();
                        stats.push(("WAITS", format!("{cnt}×"), col::DANGER));
                        stats.push(("PEAK", ins.metric.clone(), col::WARN));
                        note = Some("Contended mutex serializes parallel workers.".into());
                    }
                    "IO" => {
                        stats.push(("PEAK WAIT", ins.metric.clone(), col::WARN));
                        note = Some("Blocking OS syscall stalls worker thread.".into());
                    }
                    "HOT" | "TAIL" => {
                        if let Some(slot) = ins.target_slot {
                            if let Some(&fe) = self.world.slot_entity.get(&slot) {
                                if let Some(f) = self.world.fns.get(fe) {
                                    stats.push(("P50", fmt_ns(percentile(&f.samples, 0.5)), col::INK2));
                                    stats.push(("P95", fmt_ns(percentile(&f.samples, 0.95)), col::WARN));
                                    stats.push(("P99", fmt_ns(percentile(&f.samples, 0.99)), col::DANGER));
                                    stats.push(("CALLS", format!("{}×", f.calls), col::OK));
                                    if !f.file.is_empty() && f.line > 0 {
                                        loc_tag = format!("{}:{}", file_base(&f.file), f.line);
                                        code_key = Some((f.file.clone(), f.line));
                                    }
                                }
                            }
                        }
                    }
                    _ => {
                        note = Some(ins.detail.clone());
                    }
                }
            }
            HoverTarget::HeapPoint {
                ts_ms,
                retained,
                alloc_bytes,
                slope,
            } => {
                icon = Icon::Memory;
                let origin = self.world.trace_origin_ms.unwrap_or(ts_ms);
                title = format!("Heap @ +{}", fmt_span((ts_ms - origin).max(0.0)));
                badge_txt = if slope.abs() >= 1024.0 {
                    format!("{}{}/s", if slope > 0.0 { "+" } else { "" }, fmt_bytes(slope as i64))
                } else {
                    "stable".into()
                };
                badge_col = if slope > 32_768.0 { col::DANGER } else { col::SERIES2 };
                icon_col = badge_col;
                stats.push(("RETAINED", fmt_bytes(retained as i64), col::INK1));
                stats.push(("0.5s ALLOC", format!("+{}", fmt_bytes(alloc_bytes as i64)), col::SERIES2));
                stats.push(("SLOPE", badge_txt.clone(), badge_col));
                let sites = self.world.heap_sites_for_window(self.heap_window);
                let tot_ret: u64 = sites.iter().map(|s| s.retained_bytes).sum::<u64>().max(1);
                let pal = [col::FIRE, col::SERIES2, col::OK];
                for (i, s) in sites.iter().take(3).enumerate() {
                    let frac = (s.retained_bytes as f64 / tot_ret as f64).clamp(0.0, 1.0) as f32;
                    let lbl = if s.top_fn.is_empty() { s.key.clone() } else { s.top_fn.clone() };
                    bars.push((lbl, fmt_bytes(s.retained_bytes as i64), frac, pal[i % pal.len()]));
                }
                note = Some("Drag horizontally to slice & filter Heap below.".into());
            }
            HoverTarget::StallPoint {
                ts_ms,
                lock_ns,
                io_ns,
            } => {
                icon = Icon::Lock;
                let origin = self.world.trace_origin_ms.unwrap_or(ts_ms);
                let tot = lock_ns.saturating_add(io_ns).max(1);
                title = format!("Stalls @ +{}", fmt_span((ts_ms - origin).max(0.0)));
                badge_txt = fmt_ns(lock_ns.saturating_add(io_ns));
                badge_col = if lock_ns > 0 { col::DANGER } else { col::WARN };
                icon_col = badge_col;
                bars.push(("Mutex wait".into(), fmt_ns(lock_ns), (lock_ns as f64 / tot as f64) as f32, col::DANGER));
                bars.push(("Blocking I/O".into(), fmt_ns(io_ns), (io_ns as f64 / tot as f64) as f32, col::WARN));
                if let Some(last_lock) = self
                    .world
                    .io_rows
                    .iter()
                    .rev()
                    .find(|r| r.kind == "sync_wait" && r.holder_thread > 0)
                {
                    chips.push((
                        format!("t#{} ← t#{} ({})", last_lock.thread, last_lock.holder_thread, self.world.pc_label(last_lock.holder_pc)),
                        col::DANGER,
                    ));
                }
            }
            HoverTarget::Port(s) => {
                icon = Icon::Layers;
                icon_col = col::OK;
                title = format!("{} {}", s.proto, s.local);
                let is_listen = s.state.eq_ignore_ascii_case("LISTEN");
                badge_txt = s.state.clone();
                badge_col = if is_listen { col::OK } else { col::SERIES2 };
                stats.push(("PID", self.world.resources.pid.to_string(), col::INK1));
                stats.push(("OPEN FDS", self.world.resources.fd_count.to_string(), col::INK2));
                stats.push(("SOCKETS", self.world.resources.sockets.len().to_string(), col::OK));
                if !s.remote.is_empty() {
                    chips.push((format!("peer {}", s.remote), col::SERIES2));
                }
            }
            HoverTarget::MapCard(slot) => {
                icon = Icon::Flame;
                if let Some(&fe) = self.world.slot_entity.get(&slot) {
                    if let Some(f) = self.world.fns.get(fe) {
                        title = f.name.clone();
                        let self_ns = f.total_ns.saturating_sub(f.child_ns);
                        stats.push(("SELF", fmt_ns(self_ns), col::FIRE));
                        stats.push(("TOTAL", fmt_ns(f.total_ns), col::INK1));
                        stats.push(("P95", fmt_ns(percentile(&f.samples, 0.95)), col::WARN));
                        stats.push(("CALLS", format!("{}×", f.calls), col::OK));
                        if !f.file.is_empty() && f.line > 0 {
                            loc_tag = format!("{}:{}", file_base(&f.file), f.line);
                            code_key = Some((f.file.clone(), f.line));
                        }
                        if f.lock_contended {
                            badge_txt = "LOCK".into();
                            badge_col = col::DANGER;
                        } else if f.last_cpu != CPU_UNKNOWN {
                            let is_e = (f.last_flags & 0x10) != 0;
                            badge_txt = format!("C{} · {}", f.last_cpu, if is_e { "E" } else { "P" });
                            badge_col = if is_e { col::SERIES2 } else { col::OK };
                        }
                    } else {
                        title = format!("slot #{slot}");
                    }
                } else {
                    title = format!("slot #{slot}");
                }
            }
            HoverTarget::HeapSite(s) => {
                icon = Icon::Memory;
                title = if s.top_fn.is_empty() {
                    "Heap site".into()
                } else {
                    s.top_fn.clone()
                };
                let live_cnt = s.allocs.saturating_sub(s.frees);
                let survival = if s.allocs > 0 {
                    live_cnt as f64 / s.allocs as f64
                } else {
                    0.0
                };
                if s.allocs >= 4 && survival >= 0.85 && s.retained_bytes > 0 {
                    badge_txt = "LEAK".into();
                    badge_col = col::DANGER;
                } else if s.retained_bytes > 0 && survival >= 0.45 {
                    badge_txt = "GROWING".into();
                    badge_col = col::WARN;
                } else if s.allocs >= 6 && s.retained_bytes == 0 {
                    badge_txt = "CHURN".into();
                    badge_col = col::SERIES2;
                } else {
                    badge_txt = "OK".into();
                    badge_col = col::OK;
                }
                icon_col = badge_col;
                if !s.file.is_empty() && s.line > 0 {
                    loc_tag = format!("{}:{}", file_base(&s.file), s.line);
                    code_key = Some((s.file.clone(), s.line));
                }
                let avg = if s.allocs > 0 { s.total_bytes / s.allocs } else { 0 };
                stats.push(("RETAINED", fmt_bytes(s.retained_bytes as i64), if s.retained_bytes > 0 { col::INK1 } else { col::INK3 }));
                stats.push(("AVG BLOCK", fmt_bytes(avg as i64), col::INK1));
                stats.push(("ALLOC/FREE", format!("{} / {}", s.allocs, s.frees), col::INK2));
                stats.push(("THREAD", format!("t#{}", s.thread), col::SERIES2));

                let surv_lbl = if live_cnt == 0 {
                    "0% live (all freed)".into()
                } else {
                    format!("{:.0}% live ({live_cnt} unfreed)", survival * 100.0)
                };
                bars.push(("Survival".into(), surv_lbl, survival as f32, badge_col));

                // Call-stack breadcrumb chips (`[cache::scan_buckets] → [index::query_postings] → ...`)
                chips_is_chain = true;
                for frame in s.display_stack.split(" → ").take(3) {
                    if !frame.is_empty() {
                        chips.push((frame.to_string(), col::INK2));
                    }
                }
            }
            HoverTarget::CodeSym(sym) => {
                icon = Icon::Code;
                title = sym.name.clone();
                badge_txt = sym.kind.to_ascii_uppercase();
                badge_col = match sym.kind.as_str() {
                    "alloc" => col::FIRE,
                    "param" | "local" | "self" | "field" => col::SERIES2,
                    "fn" | "method" => col::OK,
                    "struct" | "enum" | "variant" => col::WARN,
                    _ => col::INK2,
                };
                icon_col = badge_col;
                if !sym.file.is_empty() && sym.line > 0 {
                    loc_tag = format!("{}:{}", file_base(&sym.file), sym.line);
                }
                if !sym.sig.is_empty() {
                    rust_sig = Some(sym.sig.clone());
                }
                if !sym.doc.is_empty() {
                    note = Some(sym.doc.clone());
                }
                if !sym.snippet.is_empty() {
                    inline_snippet = Some((sym.snippet.iter().take(5).cloned().collect(), sym.line));
                }
            }
        }

        if let Some((ref file, line)) = code_key {
            self.queue_source_if_needed(file, line);
        }
        let code_peek: Option<(Vec<(u32, String)>, u32)> = if let Some(s) = inline_snippet {
            Some(s)
        } else if let Some((ref file, line)) = code_key {
            self.world.snippet_for(file, line, 5).map(|v| (v, line))
        } else {
            None
        };

        // Compute clean card width from primitives
        let has_code = code_peek.as_ref().map(|(v, _)| !v.is_empty()).unwrap_or(false);
        let badge_w = if !badge_txt.is_empty() {
            self.draw.measure_text(&badge_txt, 8.8) + 18.0
        } else {
            0.0
        };
        let loc_w = if !loc_tag.is_empty() {
            self.draw.measure_text(&loc_tag, 9.0) + 12.0
        } else {
            0.0
        };
        let mut ideal_w = self.draw.measure_text(&title, 11.2) + badge_w + loc_w + 46.0;
        if let Some(ref sig) = rust_sig {
            ideal_w = ideal_w.max(self.draw.measure_text(sig, 9.8) + 34.0);
        }
        if !stats.is_empty() {
            ideal_w = ideal_w.max((stats.len() as f32) * 96.0 + 24.0);
        }
        if !chips.is_empty() {
            let mut cw = 24.0;
            for (i, (c, _)) in chips.iter().enumerate() {
                cw += self.draw.measure_text(c, 9.0) + 16.0;
                if chips_is_chain && i + 1 < chips.len() {
                    cw += 14.0;
                }
            }
            ideal_w = ideal_w.max(cw.min(540.0));
        }
        if let Some(ref n) = note {
            ideal_w = ideal_w.max(self.draw.measure_text(n, 9.4) + 28.0);
        }
        if let Some((ref cl, _)) = code_peek {
            for (_, text) in cl {
                ideal_w = ideal_w.max(self.draw.measure_text(text, 9.6) + 52.0);
            }
        }
        let max_card_w = (css_w - 24.0).clamp(320.0, 540.0);
        let min_card_w = (if has_code || !stats.is_empty() { 380.0_f32 } else { 300.0_f32 }).min(max_card_w);
        let card_w = ideal_w.clamp(min_card_w, max_card_w);
        let inner_w = (card_w - 20.0).max(160.0);

        let wrapped_note: Vec<String> = note
            .as_deref()
            .map(|n| wrap_hover_line(&self.draw, n, 9.4, inner_w))
            .unwrap_or_default();

        // Compute exact height from active primitives
        let mut zone1_h = 30.0; // header
        if rust_sig.is_some() {
            zone1_h += 28.0;
        }
        if !stats.is_empty() {
            zone1_h += 44.0;
        }
        if !bars.is_empty() {
            zone1_h += (bars.len() as f32) * 20.0 + 4.0;
        }
        if !chips.is_empty() {
            zone1_h += 24.0;
        }
        if !wrapped_note.is_empty() {
            zone1_h += (wrapped_note.len() as f32) * 14.5 + 4.0;
        }
        zone1_h += 6.0;

        let zone2_h = if let Some((ref cl, _)) = code_peek {
            if cl.is_empty() { 0.0 } else { 12.0 + (cl.len() as f32) * 15.0 }
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
            .rrect(cx + 3.0, cy + 5.0, card_w, card_h, 8.0, [0.0, 0.0, 0.0, 0.58]);
        self.draw
            .card(cx, cy, card_w, card_h, 8.0, [0.068, 0.068, 0.074, 0.99], col::LINE2);

        // 1. Header primitive: Icon + Title + right loc_tag + status badge
        let mut y = cy + 9.0;
        self.draw.icon(icon, cx + 10.0, y + 0.5, 12.0, icon_col);
        let mut rx_right = cx + card_w - 10.0;
        if !badge_txt.is_empty() {
            let bw = self.draw.measure_text(&badge_txt, 8.8) + 12.0;
            rx_right -= bw;
            self.draw.badge(
                rx_right,
                y - 1.5,
                16.0,
                8.8,
                &badge_txt,
                badge_col,
                col::with_alpha(badge_col, 0.14),
                col::with_alpha(badge_col, 0.45),
            );
            rx_right -= 8.0;
        }
        if !loc_tag.is_empty() {
            let lw = self.draw.measure_text(&loc_tag, 9.0);
            if rx_right - lw > cx + 96.0 {
                rx_right -= lw;
                self.draw.text(rx_right, y + 1.0, 9.0, col::INK4, &loc_tag);
                rx_right -= 8.0;
            }
        }
        self.draw.text_max(
            cx + 27.0,
            y - 0.5,
            11.0,
            col::INK1,
            &title,
            (rx_right - (cx + 27.0)).max(60.0),
        );
        y += 22.0;

        // 2. Optional syntax-highlighted Rust signature strip
        if let Some(ref sig) = rust_sig {
            self.draw
                .card(cx + 10.0, y, inner_w, 22.0, 4.0, [0.050, 0.050, 0.056, 1.0], col::LINE);
            self.draw
                .rust_code_line(cx + 16.0, y + 4.5, 9.6, inner_w - 12.0, sig);
            y += 28.0;
        }

        // 3. KPI Stat Tile Grid primitive
        if !stats.is_empty() {
            let th = 36.0;
            self.draw
                .card(cx + 10.0, y, inner_w, th, 5.0, col::BG2, col::LINE);
            let cell_w = inner_w / stats.len() as f32;
            for (i, (lbl, val, c)) in stats.iter().enumerate() {
                let sx = cx + 10.0 + i as f32 * cell_w;
                if i > 0 {
                    self.draw.rect(sx, y + 4.0, 1.0, th - 8.0, col::LINE);
                }
                self.draw.text(sx + 8.0, y + 5.0, 8.0, col::INK4, lbl);
                self.draw
                    .text_max(sx + 8.0, y + 17.5, 10.4, *c, val, cell_w - 14.0);
            }
            y += th + 8.0;
        }

        // 4. Visual Meter / Horizontal Bar primitive
        if !bars.is_empty() {
            for (lbl, right, frac, c) in &bars {
                let lw = (inner_w * 0.34).clamp(76.0, 150.0);
                let rw = self.draw.measure_text(right, 9.2);
                let bx = cx + 10.0 + lw + 8.0;
                let bw = (cx + 10.0 + inner_w - rw - 8.0 - bx).max(36.0);
                self.draw.text_max(cx + 10.0, y + 1.5, 9.2, col::INK2, lbl, lw);
                self.draw.rrect(bx, y + 5.5, bw, 5.5, 2.5, col::BG4);
                if *frac > 0.005 {
                    self.draw
                        .rrect(bx, y + 5.5, (bw * frac.clamp(0.0, 1.0)).max(3.5), 5.5, 2.5, *c);
                }
                self.draw
                    .text(cx + 10.0 + inner_w - rw, y + 1.5, 9.2, col::INK1, right);
                y += 20.0;
            }
            y += 4.0;
        }

        // 5. Pill / Call-Stack Breadcrumb Chips primitive
        if !chips.is_empty() {
            let mut px = cx + 10.0;
            let limit_x = cx + 10.0 + inner_w;
            for (i, (txt, c)) in chips.iter().enumerate() {
                let cw = (self.draw.measure_text(txt, 8.8) + 12.0).min(limit_x - px);
                if cw < 24.0 {
                    break;
                }
                self.draw.rrect(px, y, cw, 17.0, 4.0, col::BG3);
                self.draw.border(px, y, cw, 17.0, 4.0, 1.0, col::LINE);
                self.draw
                    .text_max(px + 6.0, y + 2.5, 8.8, *c, txt, cw - 12.0);
                px += cw + 4.0;
                if chips_is_chain && i + 1 < chips.len() && px + 14.0 < limit_x {
                    self.draw.text(px + 1.0, y + 2.0, 9.0, col::INK4, "→");
                    px += 13.0;
                }
            }
            y += 24.0;
        }

        // 6. Optional compact micro-caption
        for line in &wrapped_note {
            self.draw.text_max(cx + 10.0, y, 9.3, col::INK3, line, inner_w);
            y += 14.5;
        }

        // 7. 5-Line Syntax-Highlighted Rust Source Peek Zone
        if let Some((cl, target_line)) = code_peek {
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
                        9.2,
                        if is_t { col::FIRE } else { col::INK4 },
                        &format!("{n:>4}"),
                    );
                    self.draw
                        .rust_code_line(cx + 38.0, ly + 1.0, 9.6, card_w - 46.0, &text);
                    ly += 15.0;
                }
            }
        }
        self.draw.end_overlay();
    }
}

fn insight_icon(cat: &str) -> Icon {
    match cat {
        "HOT" => Icon::Flame,
        "TAIL" => Icon::Pulse,
        "LOCK" | "IO" | "FD" => Icon::Lock,
        "HEAP" => Icon::Memory,
        "CPU" | "ECORE" => Icon::Cpu,
        _ => Icon::Layers,
    }
}

fn insight_action(ins: &Insight) -> Action {
    if let Some(slot) = ins.target_slot {
        return Action::InspectSlot(slot);
    }
    let pane = match ins.target_tab {
        2 => PaneId::CpuTopology,
        3 => PaneId::HotSpots,
        4 => PaneId::HeapDiff,
        5 => PaneId::Builds,
        _ => PaneId::IoLocks,
    };
    Action::OpenPane(pane)
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

fn wrap_hover_line(draw: &DrawList, text: &str, size: f32, max_w: f32) -> Vec<String> {
    if text.is_empty() || draw.measure_text(text, size) <= max_w {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let candidate = if cur.is_empty() {
            word.to_string()
        } else {
            format!("{cur} {word}")
        };
        if !cur.is_empty() && draw.measure_text(&candidate, size) > max_w {
            out.push(cur);
            cur = word.to_string();
        } else {
            cur = candidate;
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn file_base(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}
