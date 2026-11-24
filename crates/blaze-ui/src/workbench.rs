//! Self-hosting live interactive **Blaze UI + Relational ECS WebGPU Workbench**.
//!
//! Built 100% with [`UiContext`]:
//! - Top toolbar with live theme preset buttons (`Obsidian`, `Paper`, `Ember`) demonstrating
//!   instant `IsA` Style Prefab cascading across every widget.
//! - Modular binary-tree docked panes (`Hierarchy`, `GPU Canvas`, `Shader IR`, `Inspector`,
//!   `Theme Prefabs`) with draggable splitters, drag-to-dock tabs, and maximize/restore.
//! - Live UI self-inspection: clicking any row in `Hierarchy` or any node in `GPU Canvas` sets
//!   `(Inspects, target)` on the Inspector pane so you can inspect and live-edit the UI's own
//!   relational ECS entities at 120 FPS.

use core::alloc::Allocator;
use core::fmt::Write as _;

use blaze_ecs::{Entity, IncrementalCount, Signal};
use blaze_own::{String as OwnString, Vec as OwnVec};

use crate::components::{
    BadgeTone, CanvasBatch, ComputedRect, DockedIn, Focusable, Focused, Hovered, Inspects,
    Interactive, Selected, SliderState, TextInputState, TreeRowState, WidgetKind,
};
use crate::dock::SplitDir;
use crate::draw::DrawList;
use crate::error::UiError;
use crate::input::{InputEvent, UiEvent};
use crate::shaders::{
    emit_ui_glyph_msl, emit_ui_glyph_wgsl, emit_ui_sdf_msl, emit_ui_sdf_wgsl,
};
use crate::style::{Align, LayoutStyle, PaintStyle, Sizing};
use crate::types::{Edges, Rect, Rgba, ThemeKind};
use crate::ui::UiContext;
use crate::virtual_list::VirtualList;

const TOOLBAR_H: f32 = 38.0;
const STATUSBAR_H: f32 = 24.0;

// Semantic ActionTag ranges
const ACT_THEME_OBSIDIAN: u32 = 100;
const ACT_THEME_PAPER: u32 = 101;
const ACT_THEME_EMBER: u32 = 102;
const ACT_RESET_DOCK: u32 = 103;
const ACT_SEARCH_INPUT: u32 = 110;
const ACT_CANVAS: u32 = 120;
const ACT_LAB_EDIT_ONE: u32 = 150;
const ACT_LAB_NOOP_WRITE: u32 = 151;
const ACT_LAB_TOGGLE_UNREAD: u32 = 152;
const ACT_LAB_SCROLL_100K: u32 = 153;
const ACT_PREFAB_PANEL_RADIUS: u32 = 200;
const ACT_PREFAB_BTN_RADIUS: u32 = 201;
const ACT_PREFAB_SHADOW: u32 = 202;
const ACT_TOGGLE_ANIMATE: u32 = 203;
const ACT_TOGGLE_SHOW_PREFABS: u32 = 204;
const ACT_INSP_RADIUS: u32 = 210;
const ACT_INSP_BORDER: u32 = 211;
const ACT_TREE_ROW_BASE: u32 = 10_000;
const ACT_VLIST_ROW_BASE: u32 = 20_000;

/// Live self-hosting Blaze UI + ECS Workbench application.
pub struct WorkbenchApp<A: Allocator + Copy + 'static> {
    pub ui: UiContext<A>,
    pub inspected: Entity,
    inspector_pane: Entity,
    hierarchy_list: Entity,
    inspector_props: Entity,
    shader_lines_box: Entity,
    search_input: Entity,
    canvas_ent: Entity,
    badge_entities: Entity,
    badge_clips: Entity,
    badge_instances: Entity,
    status_label: Entity,
    insp_slider_radius: Entity,
    insp_slider_border: Entity,
    prefab_slider_panel_r: Entity,
    prefab_slider_btn_r: Entity,
    prefab_slider_shadow: Entity,
    // Dock pane roots (retained across Reset Dock)
    pane_hierarchy: Entity,
    pane_canvas: Entity,
    pane_shaders: Entity,
    pane_lab: Entity,
    pane_inspector: Entity,
    pane_prefabs: Entity,
    // Reactivity & 100k VirtualList Lab state (run-once reactive holes + O(1) IncrementalCount)
    lab_vlist: VirtualList<A>,
    lab_unread: IncrementalCount<u32, A>,
    lab_edit_count: Signal<u32>,
    lab_stat_row: Entity,
    lab_scroll_row: Entity,
    lab_toggle_cursor: u32,
    lab_edit_seq: u32,
    // Interactive state
    animate_graph: bool,
    show_prefabs_in_tree: bool,
    canvas_zoom: f32,
    tree_entities: OwnVec<Entity, A>,
    graph_nodes: OwnVec<(Entity, f32, f32, f32), A>,
    events_scratch: OwnVec<UiEvent, A>,
    last_w: f32,
    last_h: f32,
    last_time: f32,
    dirty_tree: bool,
    dirty_inspector: bool,
    telemetry_syncing: bool,
}

impl<A: Allocator + Copy + 'static> WorkbenchApp<A> {
    /// Construct a fresh Workbench app backed by explicit allocator `alloc`.
    pub fn new_in(alloc: A) -> Result<Self, UiError> {
        let mut ui = UiContext::new_in(alloc)?;

        // Measure Rust-authored shader outputs once for the Shader IR pane.
        let mut sdf_wgsl = OwnString::new_in(alloc);
        emit_ui_sdf_wgsl(&mut sdf_wgsl)?;
        let sdf_wgsl_bytes = sdf_wgsl.len();
        let mut sdf_msl = OwnString::new_in(alloc);
        emit_ui_sdf_msl(&mut sdf_msl)?;
        let sdf_msl_bytes = sdf_msl.len();
        let mut glyph_wgsl = OwnString::new_in(alloc);
        emit_ui_glyph_wgsl(&mut glyph_wgsl)?;
        let glyph_wgsl_bytes = glyph_wgsl.len();
        let mut glyph_msl = OwnString::new_in(alloc);
        emit_ui_glyph_msl(&mut glyph_msl)?;
        let glyph_msl_bytes = glyph_msl.len();

        // 1. Top Toolbar
        let toolbar = ui.spawn_toolbar(ui.root, "TopToolbar", TOOLBAR_H)?;
        let _ = ui.spawn_badge(toolbar, "BrandBadge", "BLAZE UI", BadgeTone::Fire)?;
        let _ = ui.spawn_badge(toolbar, "ArchBadge", "ECS -> WebGPU", BadgeTone::Blue)?;
        let _ = ui.spawn_button(toolbar, "BtnObsidian", "Obsidian", ACT_THEME_OBSIDIAN, false)?;
        let _ = ui.spawn_button(toolbar, "BtnPaper", "Paper", ACT_THEME_PAPER, false)?;
        let _ = ui.spawn_button(toolbar, "BtnEmber", "Ember", ACT_THEME_EMBER, false)?;
        let _ = ui.spawn_spacer(toolbar)?;
        let badge_entities = ui.spawn_badge(toolbar, "StatEntities", "0 ent", BadgeTone::Neutral)?;
        ui.world.set_copy(badge_entities, LayoutStyle::row().width(Sizing::Fixed(68.0)).height(Sizing::Fixed(20.0)))?;
        let badge_clips = ui.spawn_badge(toolbar, "StatClips", "0 clips", BadgeTone::Ok)?;
        ui.world.set_copy(badge_clips, LayoutStyle::row().width(Sizing::Fixed(72.0)).height(Sizing::Fixed(20.0)))?;
        let badge_instances = ui.spawn_badge(toolbar, "StatInst", "0 inst", BadgeTone::Violet)?;
        ui.world.set_copy(badge_instances, LayoutStyle::row().width(Sizing::Fixed(148.0)).height(Sizing::Fixed(20.0)))?;
        let _ = ui.spawn_button(toolbar, "BtnResetDock", "Reset Dock", ACT_RESET_DOCK, false)?;

        // Center placeholder flex region where DockWorkspace renders, then bottom StatusBar.
        let _dock_spacer = ui.spawn_container(
            ui.root,
            "DockViewportSlot",
            WidgetKind::Container,
            LayoutStyle::column().fill(),
            None,
        )?;
        let statusbar = ui.spawn_toolbar(ui.root, "StatusBar", STATUSBAR_H)?;
        let status_label = ui.spawn_label(
            statusbar,
            "StatusText",
            "Rust gpu::emit -> WGSL + MSL 2.4 · 0 scissor breaks · Drag any tab to split/dock · Click any entity to self-inspect",
            true,
        )?;

        // 2. Pane 1: Hierarchy (Self-Inspecting UI ECS Tree)
        let pane_hierarchy = ui.spawn_pane_root("Pane::Hierarchy", false)?;
        let search_input = ui.spawn_text_input(
            pane_hierarchy,
            "HierarchySearch",
            "",
            "Filter UI ECS entities...",
            ACT_SEARCH_INPUT,
        )?;
        let hierarchy_list = ui.spawn_container(
            pane_hierarchy,
            "HierarchyList",
            WidgetKind::Container,
            LayoutStyle::column()
                .fill()
                .pad(Edges::new(0.0, 8.0, 0.0, 0.0))
                .gap(2.0)
                .scrollable(true),
            None,
        )?;

        // 3. Pane 2: Hybrid Dense GPU Canvas (Flamegraph + Live ECS Relation Graph)
        let pane_canvas = ui.spawn_pane_root("Pane::GpuCanvas", false)?;
        let canvas_ent = ui.spawn_canvas(pane_canvas, "HybridGpuCanvas", ACT_CANVAS)?;

        // 4. Pane 3: Shader IR & Generated WGSL Preview
        let pane_shaders = ui.spawn_pane_root("Pane::ShaderIR", true)?;
        let sh_card = ui.spawn_card(
            pane_shaders,
            "ShaderSummaryCard",
            LayoutStyle::column().width(Sizing::FILL).pad(Edges::all(8.0)).gap(4.0),
        )?;
        let _ = ui.spawn_label(sh_card, "ShTitle", "Rust-Authored Shaders (crates/gpu -> WGSL & MSL 2.4)", false)?;
        let mut s_buf = OwnString::new_in(alloc);
        let _ = write!(s_buf, "{sdf_wgsl_bytes} B WGSL / {sdf_msl_bytes} B MSL 2.4");
        let _ = ui.spawn_property_row(sh_card, "ShRowSdf", "ui_sdf_vs/fs", s_buf.as_str())?;
        s_buf.clear();
        let _ = write!(s_buf, "{glyph_wgsl_bytes} B WGSL / {glyph_msl_bytes} B MSL 2.4");
        let _ = ui.spawn_property_row(sh_card, "ShRowGlyph", "ui_glyph_vs/fs", s_buf.as_str())?;
        let _ = ui.spawn_property_row(sh_card, "ShRowClip", "Clip Strategy", "clips[clip_id] SDF in fragment (0 scissor breaks)")?;
        let _ = ui.spawn_property_row(sh_card, "ShRowDraws", "Draw Calls", "2 instanced base + 2 overlay (120 FPS)")?;

        let shader_lines_box = ui.spawn_container(
            pane_shaders,
            "WgslPreviewBox",
            WidgetKind::Card,
            LayoutStyle::column().width(Sizing::FILL).pad(Edges::new(8.0, 10.0, 12.0, 10.0)).gap(2.0),
            Some(ui.prefabs.panel),
        )?;
        let _ = ui.spawn_label(shader_lines_box, "WgslHead", "Generated WGSL (ui_sdf_vs + ui_sdf_fs):", true)?;
        for (idx, line) in sdf_wgsl.as_str().lines().take(34).enumerate() {
            let mut nm = OwnString::new_in(alloc);
            let _ = write!(nm, "WgslL{idx}");
            let _ = ui.spawn_label(shader_lines_box, nm.as_str(), line, false)?;
        }

        // 4b. Pane 3b: Reactivity & 100K VirtualList Lab (run-once reactive holes + O(1) IncrementalCount)
        let pane_lab = ui.spawn_pane_root("Pane::ReactivityLab", false)?;
        let lab_bar = ui.spawn_container(
            pane_lab,
            "LabActionBar",
            WidgetKind::Container,
            LayoutStyle::row().width(Sizing::FILL).height(Sizing::Fixed(28.0)).gap(6.0).align(Align::Center),
            None,
        )?;
        let _ = ui.spawn_button(lab_bar, "LabEditOne", "Edit 1 Label", ACT_LAB_EDIT_ONE, true)?;
        let _ = ui.spawn_button(lab_bar, "LabNoop", "Equal Write (0B)", ACT_LAB_NOOP_WRITE, false)?;
        let _ = ui.spawn_button(lab_bar, "LabToggleUnread", "Toggle Unread", ACT_LAB_TOGGLE_UNREAD, false)?;
        let _ = ui.spawn_button(lab_bar, "LabScroll100k", "Scroll +500", ACT_LAB_SCROLL_100K, false)?;
        let _ = ui.spawn_spacer(lab_bar)?;
        let lab_unread_badge = ui.spawn_badge(lab_bar, "LabUnreadBadge", "0 unread", BadgeTone::Fire)?;
        ui.world.set_copy(
            lab_unread_badge,
            LayoutStyle::row().width(Sizing::Fixed(86.0)).height(Sizing::Fixed(20.0)),
        )?;

        let lab_metrics = ui.spawn_card(
            pane_lab,
            "LabMetricsCard",
            LayoutStyle::column().width(Sizing::FILL).pad(Edges::all(8.0)).gap(3.0),
        )?;
        let lab_stat_row = ui.spawn_property_row(
            lab_metrics,
            "LabStatRow",
            "Last Frame Delta",
            "layout: 0 · paint: 0 · patch: 0 B",
        )?;
        let lab_scroll_row = ui.spawn_property_row(
            lab_metrics,
            "LabScrollRow",
            "100,000-Row Pool",
            "24 pooled ECS entities · row 0..24",
        )?;

        let lab_edit_count = ui.signal(0u32)?;
        let mut lab_unread = IncrementalCount::<u32, A>::new_in(&mut ui.reactive, alloc)?;
        for i in 0u32..32u32 {
            let _ = lab_unread.upsert(&mut ui.reactive, i, i % 3 == 0)?;
        }
        let unread_sig = lab_unread.signal();
        // Run-once reactive hole: badge updates ONLY when `IncrementalCount`'s signal changes!
        ui.bind_label(lab_unread_badge, move |cx, out| {
            let n = cx.val(unread_sig);
            let _ = write!(out, "{n} unread");
        })?;
        // Run-once reactive hole for edit counter row
        let lab_edit_row = ui.spawn_property_row(lab_metrics, "LabEditRow", "Reactive Signal", "edits: 0")?;
        ui.bind_sub_text(lab_edit_row, move |cx, out| {
            let n = cx.val(lab_edit_count);
            let _ = write!(out, "edits: {n} (run-once reactive hole)");
        })?;

        let lab_vlist_box = ui.spawn_container(
            pane_lab,
            "LabVirtualListBox",
            WidgetKind::Card,
            LayoutStyle::column().fill().pad(Edges::all(6.0)).gap(2.0).clip(true),
            Some(ui.prefabs.panel),
        )?;
        let mut lab_vlist = VirtualList::new_fixed_in(100_000, 22.0, alloc);
        lab_vlist.mount_pool(&mut ui, lab_vlist_box, "VRow", 24, ACT_VLIST_ROW_BASE)?;
        let _ = lab_vlist.sync_visible(&mut ui, 24.0 * 22.0, |idx, lbl, sub| {
            let _ = write!(lbl, "Row #{idx:06} · ECS Signal Entity");
            let _ = write!(sub, "{}", if idx % 3 == 0 { "unread" } else { "read" });
        })?;

        // 5. Pane 4: Live Entity Inspector (`Inspects` relation!)
        let pane_inspector = ui.spawn_pane_root("Pane::Inspector", true)?;
        let inspector_props = ui.spawn_container(
            pane_inspector,
            "InspectorProps",
            WidgetKind::Container,
            LayoutStyle::column().width(Sizing::FILL).gap(3.0),
            None,
        )?;
        let insp_card = ui.spawn_card(
            pane_inspector,
            "InspectorStyleCard",
            LayoutStyle::column().width(Sizing::FILL).pad(Edges::all(8.0)).gap(4.0),
        )?;
        let _ = ui.spawn_label(insp_card, "InspStyleTitle", "Live Component Override (PaintStyle)", false)?;
        let insp_slider_radius = ui.spawn_slider(
            insp_card,
            "InspSliderRadius",
            "Corner Radius",
            SliderState::new(6.0, 0.0, 18.0, 0.5, 1),
            ACT_INSP_RADIUS,
        )?;
        let insp_slider_border = ui.spawn_slider(
            insp_card,
            "InspSliderBorder",
            "Border Width",
            SliderState::new(1.0, 0.0, 4.0, 0.25, 2),
            ACT_INSP_BORDER,
        )?;

        // 6. Pane 5: Live `IsA` Theme Prefab Cascade Controls
        let pane_prefabs = ui.spawn_pane_root("Pane::ThemePrefabs", true)?;
        let pf_card = ui.spawn_card(
            pane_prefabs,
            "PrefabCascadeCard",
            LayoutStyle::column().width(Sizing::FILL).pad(Edges::all(8.0)).gap(6.0),
        )?;
        let _ = ui.spawn_label(pf_card, "PfTitle", "Live IsA Style Prefab Cascade", false)?;
        let _ = ui.spawn_label(
            pf_card,
            "PfSub",
            "Mutating 1 Style::* Prefab restyles every inheriting entity in O(1)",
            true,
        )?;
        let prefab_slider_panel_r = ui.spawn_slider(
            pf_card,
            "PfPanelRadius",
            "Panel Radius",
            SliderState::new(ui.theme.radius_md, 0.0, 16.0, 0.5, 1),
            ACT_PREFAB_PANEL_RADIUS,
        )?;
        let prefab_slider_btn_r = ui.spawn_slider(
            pf_card,
            "PfBtnRadius",
            "Button Radius",
            SliderState::new(ui.theme.radius_sm, 0.0, 14.0, 0.5, 1),
            ACT_PREFAB_BTN_RADIUS,
        )?;
        let prefab_slider_shadow = ui.spawn_slider(
            pf_card,
            "PfShadowBlur",
            "Card Shadow",
            SliderState::new(6.0, 0.0, 22.0, 1.0, 0),
            ACT_PREFAB_SHADOW,
        )?;
        let _ = ui.spawn_toggle(pf_card, "PfToggleAnim", "Animate Graph Pulse", false, ACT_TOGGLE_ANIMATE)?;
        let _ = ui.spawn_toggle(
            pf_card,
            "PfTogglePrefabs",
            "Show Style Prefabs in Tree",
            true,
            ACT_TOGGLE_SHOW_PREFABS,
        )?;

        let inspected = ui.prefabs.button;
        ui.world.relate::<Inspects>(pane_inspector, inspected)?;

        let mut app = Self {
            ui,
            inspected,
            inspector_pane: pane_inspector,
            hierarchy_list,
            inspector_props,
            shader_lines_box,
            search_input,
            canvas_ent,
            badge_entities,
            badge_clips,
            badge_instances,
            status_label,
            insp_slider_radius,
            insp_slider_border,
            prefab_slider_panel_r,
            prefab_slider_btn_r,
            prefab_slider_shadow,
            pane_hierarchy,
            pane_canvas,
            pane_shaders,
            pane_lab,
            pane_inspector,
            pane_prefabs,
            lab_vlist,
            lab_unread,
            lab_edit_count,
            lab_stat_row,
            lab_scroll_row,
            lab_toggle_cursor: 0,
            lab_edit_seq: 0,
            animate_graph: false,
            show_prefabs_in_tree: true,
            canvas_zoom: 1.0,
            tree_entities: OwnVec::new_in(alloc),
            graph_nodes: OwnVec::new_in(alloc),
            events_scratch: OwnVec::new_in(alloc),
            last_w: 1280.0,
            last_h: 800.0,
            last_time: 0.0,
            dirty_tree: true,
            dirty_inspector: true,
            telemetry_syncing: false,
        };

        app.build_default_dock()?;
        app.rebuild_hierarchy()?;
        app.rebuild_inspector()?;
        Ok(app)
    }

    /// Build the default 3-column binary-tree dock layout:
    /// `[ Hierarchy (22%) | [ GPU Canvas / Shader IR (50%) | Inspector / Theme Prefabs (28%) ] ]`.
    pub fn build_default_dock(&mut self) -> Result<(), UiError> {
        if let Some(old_root) = self.ui.dock.root.take() {
            if self.ui.world.is_alive(old_root) {
                self.ui.world.despawn(old_root)?;
            }
        }
        self.ui.dock.maximized_tab = None;
        let leaf_left = self.ui.dock.spawn_leaf(&mut self.ui.world, "Leaf::Hierarchy")?;
        let leaf_mid = self.ui.dock.spawn_leaf(&mut self.ui.world, "Leaf::Viewport")?;
        let leaf_right = self.ui.dock.spawn_leaf(&mut self.ui.world, "Leaf::Inspector")?;

        let _ = self.ui.dock.add_tab(&mut self.ui.world, leaf_left, "Hierarchy", 1, self.pane_hierarchy)?;
        let _ = self.ui.dock.add_tab(&mut self.ui.world, leaf_mid, "GPU Canvas", 2, self.pane_canvas)?;
        let _ = self.ui.dock.add_tab(&mut self.ui.world, leaf_mid, "Shader IR", 3, self.pane_shaders)?;
        let _ = self.ui.dock.add_tab(&mut self.ui.world, leaf_mid, "100K Lab", 6, self.pane_lab)?;
        let _ = self.ui.dock.add_tab(&mut self.ui.world, leaf_right, "Inspector", 4, self.pane_inspector)?;
        let _ = self.ui.dock.add_tab(&mut self.ui.world, leaf_right, "Theme Prefabs", 5, self.pane_prefabs)?;

        let split_right = self.ui.dock.spawn_split(
            &mut self.ui.world,
            "Split::MainRight",
            SplitDir::Horizontal,
            0.64,
            leaf_mid,
            leaf_right,
        )?;
        let root_split = self.ui.dock.spawn_split(
            &mut self.ui.world,
            "Split::Root",
            SplitDir::Horizontal,
            0.22,
            leaf_left,
            split_right,
        )?;
        self.ui.dock.root = Some(root_split);
        self.dirty_tree = true;
        Ok(())
    }

    /// Select `target` for live inspection via the `(Inspects, target)` ECS relation.
    pub fn select_entity(&mut self, target: Entity) -> Result<(), UiError> {
        if !self.ui.world.is_alive(target) {
            return Ok(());
        }
        self.inspected = target;
        self.ui.world.relate::<Inspects>(self.inspector_pane, target)?;
        self.dirty_tree = true;
        self.dirty_inspector = true;
        Ok(())
    }

    /// Feed a sans-IO [`InputEvent`] and apply resulting semantic [`UiEvent`]s.
    pub fn handle_input(&mut self, ev: InputEvent) -> Result<(), UiError> {
        self.ui.handle_input(ev)?;
        self.ui.drain_events(&mut self.events_scratch)?;

        let count = self.events_scratch.len();
        for i in 0..count {
            let Some(ue) = self.events_scratch.get(i).copied() else { continue };
            match ue {
                UiEvent::Click { entity: _, action } => match action {
                    ACT_THEME_OBSIDIAN => {
                        self.ui.set_theme(ThemeKind::Obsidian)?;
                        self.sync_prefab_sliders()?;
                        self.dirty_inspector = true;
                    }
                    ACT_THEME_PAPER => {
                        self.ui.set_theme(ThemeKind::Paper)?;
                        self.sync_prefab_sliders()?;
                        self.dirty_inspector = true;
                    }
                    ACT_THEME_EMBER => {
                        self.ui.set_theme(ThemeKind::Ember)?;
                        self.sync_prefab_sliders()?;
                        self.dirty_inspector = true;
                    }
                    ACT_RESET_DOCK => {
                        self.build_default_dock()?;
                    }
                    ACT_LAB_EDIT_ONE => {
                        self.lab_edit_seq = self.lab_edit_seq.saturating_add(1);
                        let _ = self.ui.set_signal(self.lab_edit_count, self.lab_edit_seq)?;
                        if let Some(&first_row) = self.lab_vlist.pool.first() {
                            let mut txt = OwnString::new_in(self.ui.alloc());
                            let _ = write!(txt, "Row #000000 · patched edit #{}", self.lab_edit_seq);
                            let _ = self.ui.set_label_text(first_row, txt.as_str())?;
                        }
                    }
                    ACT_LAB_NOOP_WRITE => {
                        // Equality-guarded no-op write: 0 dirty entities, 0 layout, 0 GPU bytes!
                        let _ = self.ui.set_signal(self.lab_edit_count, self.lab_edit_seq)?;
                    }
                    ACT_LAB_TOGGLE_UNREAD => {
                        let k = self.lab_toggle_cursor % 32;
                        self.lab_toggle_cursor = self.lab_toggle_cursor.saturating_add(1);
                        let next_unread = !self.lab_unread.get(k);
                        let _ = self.lab_unread.upsert(&mut self.ui.reactive, k, next_unread)?;
                    }
                    ACT_LAB_SCROLL_100K => {
                        let _ = self.lab_vlist.scroll_by(500.0 * 22.0, 24.0 * 22.0);
                        let _ = self.lab_vlist.sync_visible(&mut self.ui, 24.0 * 22.0, |idx, lbl, sub| {
                            let _ = write!(lbl, "Row #{idx:06} · ECS Signal Entity");
                            let _ = write!(sub, "{}", if idx % 3 == 0 { "unread" } else { "read" });
                        })?;
                        let vis = self.lab_vlist.visible_range(24.0 * 22.0, 0);
                        let mut s = OwnString::new_in(self.ui.alloc());
                        let _ = write!(
                            s,
                            "24 pooled ECS entities · row {}..{}",
                            vis.first_row,
                            vis.first_row + vis.count
                        );
                        let _ = self.ui.set_sub_text(self.lab_scroll_row, s.as_str())?;
                    }
                    a if a >= ACT_TREE_ROW_BASE && a < ACT_VLIST_ROW_BASE => {
                        let idx = usize::try_from(a - ACT_TREE_ROW_BASE).unwrap_or(usize::MAX);
                        if let Some(&target) = self.tree_entities.get(idx) {
                            self.select_entity(target)?;
                        }
                    }
                    _ => {}
                },
                UiEvent::ToggleChanged { action, checked, .. } => match action {
                    ACT_TOGGLE_ANIMATE => self.animate_graph = checked,
                    ACT_TOGGLE_SHOW_PREFABS => {
                        self.show_prefabs_in_tree = checked;
                        self.dirty_tree = true;
                    }
                    _ => {}
                },
                UiEvent::SliderChanged { action, value, .. } => match action {
                    ACT_PREFAB_PANEL_RADIUS => {
                        let mut theme = self.ui.theme;
                        theme.radius_md = value;
                        self.ui.apply_custom_theme(theme)?;
                        self.dirty_inspector = true;
                    }
                    ACT_PREFAB_BTN_RADIUS => {
                        let mut theme = self.ui.theme;
                        theme.radius_sm = value;
                        self.ui.apply_custom_theme(theme)?;
                        self.dirty_inspector = true;
                    }
                    ACT_PREFAB_SHADOW => {
                        let mut ps = self
                            .ui
                            .world
                            .get::<PaintStyle>(self.ui.prefabs.card)?
                            .copied()
                            .unwrap_or_else(|| PaintStyle::card(&self.ui.theme));
                        ps.shadow_blur = value;
                        let _ = self.ui.set_prefab_style(self.ui.prefabs.card, ps)?;
                    }
                    ACT_INSP_RADIUS => {
                        if self.ui.world.is_alive(self.inspected) {
                            let mut ps = self
                                .ui
                                .world
                                .get_inherited::<PaintStyle>(self.inspected)?
                                .copied()
                                .unwrap_or_else(|| PaintStyle::panel(&self.ui.theme));
                            ps.radius = value;
                            if self.ui.world.has_tag(self.inspected, self.ui.world.prefab())? {
                                let _ = self.ui.set_prefab_style(self.inspected, ps)?;
                            } else {
                                let _ = self.ui.set_paint_style(self.inspected, ps)?;
                            }
                            self.dirty_inspector = true;
                        }
                    }
                    ACT_INSP_BORDER => {
                        if self.ui.world.is_alive(self.inspected) {
                            let mut ps = self
                                .ui
                                .world
                                .get_inherited::<PaintStyle>(self.inspected)?
                                .copied()
                                .unwrap_or_else(|| PaintStyle::panel(&self.ui.theme));
                            ps.border_width = value;
                            if self.ui.world.has_tag(self.inspected, self.ui.world.prefab())? {
                                let _ = self.ui.set_prefab_style(self.inspected, ps)?;
                            } else {
                                let _ = self.ui.set_paint_style(self.inspected, ps)?;
                            }
                            self.dirty_inspector = true;
                        }
                    }
                    _ => {}
                },
                UiEvent::TextChanged { action, .. } => {
                    if action == ACT_SEARCH_INPUT {
                        self.dirty_tree = true;
                    }
                }
                UiEvent::CanvasClick { local_x, local_y, .. } => {
                    let mut best: Option<(Entity, f32)> = None;
                    for &(ent, nx, ny, nr) in self.graph_nodes.as_slice() {
                        let dx = local_x - nx;
                        let dy = local_y - ny;
                        let d2 = dx * dx + dy * dy;
                        let hit_r = (nr + 8.0) * (nr + 8.0);
                        if d2 <= hit_r && best.map_or(true, |(_, bd)| d2 < bd) {
                            best = Some((ent, d2));
                        }
                    }
                    if let Some((ent, _)) = best {
                        self.select_entity(ent)?;
                    }
                }
                UiEvent::CanvasWheel { dy, .. } => {
                    let factor = if dy < 0.0 { 1.1 } else { 0.91 };
                    self.canvas_zoom = (self.canvas_zoom * factor).clamp(0.55, 2.2);
                }
                UiEvent::DockChanged | UiEvent::TabSelected { .. } => {
                    self.dirty_tree = true;
                }
            }
        }
        Ok(())
    }

    /// Render a complete frame into `self.ui.draw` (ready for 1-call WebGPU upload!).
    pub fn render_frame(&mut self, width: f32, height: f32, dpr: f32, time: f32) -> Result<(), UiError> {
        self.last_w = width.max(320.0);
        self.last_h = height.max(240.0);
        self.last_time = time;

        if self.dirty_tree {
            self.rebuild_hierarchy()?;
            self.dirty_tree = false;
        }
        if self.dirty_inspector {
            self.rebuild_inspector()?;
            self.dirty_inspector = false;
        }

        // Only repopulate the hybrid GPU canvas when visible & animated/dirty (0 µs idle frames!).
        let canvas_visible = self.is_pane_visible(self.pane_canvas)?;
        if (canvas_visible && (self.animate_graph || self.ui.last_stats.full_layout))
            || self.ui.draw.rects.is_empty()
        {
            self.populate_gpu_canvas(time)?;
            if self.animate_graph && canvas_visible {
                self.ui.mark_paint_dirty(self.canvas_ent)?;
            }
        }

        let dock_bounds = Rect::new(
            6.0,
            TOOLBAR_H + 6.0,
            (self.last_w - 12.0).max(16.0),
            (self.last_h - TOOLBAR_H - STATUSBAR_H - 12.0).max(16.0),
        );
        self.ui.render_frame(self.last_w, self.last_h, dpr, time, Some(dock_bounds))?;

        // Refresh live toolbar telemetry badges only when values actually change (equality-guarded!).
        self.update_telemetry_badges()?;
        let _ = self.shader_lines_box;
        let _ = self.status_label;
        Ok(())
    }

    fn sync_prefab_sliders(&mut self) -> Result<(), UiError> {
        self.ui.world.set_copy(
            self.prefab_slider_panel_r,
            SliderState::new(self.ui.theme.radius_md, 0.0, 16.0, 0.5, 1),
        )?;
        self.ui.world.set_copy(
            self.prefab_slider_btn_r,
            SliderState::new(self.ui.theme.radius_sm, 0.0, 14.0, 0.5, 1),
        )?;
        let sh = self
            .ui
            .world
            .get::<PaintStyle>(self.ui.prefabs.card)?
            .map_or(6.0, |p| p.shadow_blur);
        self.ui
            .world
            .set_copy(self.prefab_slider_shadow, SliderState::new(sh, 0.0, 22.0, 1.0, 0))?;
        Ok(())
    }

    fn is_pane_visible(&self, pane: Entity) -> Result<bool, UiError> {
        for leaf_r in self.ui.dock.leaves.as_slice() {
            if let Some(active_tab) = self.ui.dock.active_tab_of(&self.ui.world, leaf_r.leaf)? {
                if let Some(tab) = self.ui.world.get::<crate::dock::DockTab>(active_tab)? {
                    if tab.content == pane {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }

    fn update_telemetry_badges(&mut self) -> Result<(), UiError> {
        let st = self.ui.last_stats;
        if st.layout_nodes_visited == 0
            && st.paint_nodes_visited == 0
            && st.reactive_evals == 0
            && !st.full_extract
        {
            self.telemetry_syncing = false;
            return Ok(());
        }
        if self.telemetry_syncing && !st.full_extract && st.layout_nodes_visited == 0 {
            self.telemetry_syncing = false;
            return Ok(());
        }
        let alloc = self.ui.alloc();
        let ent_n = self.ui.world.live_count();
        let clip_n = self.ui.draw.clips.len();

        let mut any = false;
        let mut s = OwnString::new_in(alloc);
        let _ = write!(s, "{ent_n} ent");
        any |= self.ui.set_label_text(self.badge_entities, s.as_str())?;

        let mut c = OwnString::new_in(alloc);
        let _ = write!(c, "{clip_n} clips");
        any |= self.ui.set_label_text(self.badge_clips, c.as_str())?;

        let mut ins = OwnString::new_in(alloc);
        if st.full_extract {
            let _ = write!(ins, "full {}B", st.gpu_bytes_patched);
        } else {
            let _ = write!(ins, "Δ L:{} P:{} {}B", st.layout_nodes_visited, st.paint_nodes_visited, st.gpu_bytes_patched);
        }
        any |= self.ui.set_label_text(self.badge_instances, ins.as_str())?;

        if st.layout_nodes_visited > 0 || st.paint_nodes_visited > 0 || st.reactive_evals > 0 {
            let mut ls = OwnString::new_in(alloc);
            let _ = write!(
                ls,
                "layout: {} · paint: {} · patch: {} B · evals: {}",
                st.layout_nodes_visited, st.paint_nodes_visited, st.gpu_bytes_patched, st.reactive_evals
            );
            any |= self.ui.set_sub_text(self.lab_stat_row, ls.as_str())?;
        }
        self.telemetry_syncing = any;
        Ok(())
    }

    fn rebuild_hierarchy(&mut self) -> Result<(), UiError> {
        let alloc = self.ui.alloc();
        self.ui.clear_children(self.hierarchy_list)?;
        self.tree_entities.clear();

        let mut filter_lower = OwnString::new_in(alloc);
        if let Some(st) = self.ui.world.get::<TextInputState<A>>(self.search_input)? {
            for ch in st.text.as_str().chars() {
                for lc in ch.to_lowercase() {
                    filter_lower.try_push(lc)?;
                }
            }
        }
        let has_filter = !filter_lower.is_empty();

        // Collect key root subtrees to display without listing generated tree rows themselves!
        let mut stack: OwnVec<(Entity, u16), A> = OwnVec::new_in(alloc);
        if let Some(dock_root) = self.ui.dock.root {
            stack.try_push((dock_root, 0)).map_err(|r| UiError::Alloc(r.why))?;
        }
        stack.try_push((self.ui.root, 0)).map_err(|r| UiError::Alloc(r.why))?;
        if self.show_prefabs_in_tree {
            let pf = self.ui.prefabs;
            let prefabs = [
                pf.tooltip,
                pf.row,
                pf.input,
                pf.button_primary,
                pf.button,
                pf.toolbar,
                pf.card,
                pf.panel,
                pf.root,
            ];
            for e in prefabs {
                stack.try_push((e, 0)).map_err(|r| UiError::Alloc(r.why))?;
            }
        }

        let mut rows_added: usize = 0;
        let mut lower_scratch = OwnString::new_in(alloc);
        let mut row_name = OwnString::new_in(alloc);

        while let Some((ent, depth)) = stack.pop() {
            if rows_added >= 96 || !self.ui.world.is_alive(ent) {
                continue;
            }
            // Don't infinitely mirror the dynamic hierarchy rows & inspector rows inside themselves.
            if ent == self.hierarchy_list || ent == self.inspector_props || ent == self.shader_lines_box {
                continue;
            }
            let name = self.ui.world.entity_name(ent).unwrap_or("Entity");
            let kind_str = self
                .ui
                .world
                .get::<WidgetKind>(ent)?
                .map_or("Prefab", |k| k.as_str());

            let children = self.ui.world.children_of(ent)?;
            let has_children = !children.is_empty();

            let matches_filter = if !has_filter {
                true
            } else {
                lower_scratch.clear();
                for ch in name.chars() {
                    for lc in ch.to_lowercase() {
                        let _ = lower_scratch.try_push(lc);
                    }
                }
                lower_scratch.as_str().contains(filter_lower.as_str())
            };

            if matches_filter {
                let row_idx = self.tree_entities.len();
                self.tree_entities.try_push(ent).map_err(|r| UiError::Alloc(r.why))?;
                let action = ACT_TREE_ROW_BASE.saturating_add(u32::try_from(row_idx).unwrap_or(0));

                // Copy strings before mutating `self.ui`.
                let mut lbl_copy = OwnString::new_in(alloc);
                lbl_copy.try_push_str(name)?;
                row_name.clear();
                let _ = write!(row_name, "TreeRow{row_idx}");

                let row_ent = self.ui.spawn_tree_row(
                    self.hierarchy_list,
                    row_name.as_str(),
                    lbl_copy.as_str(),
                    kind_str,
                    TreeRowState { depth, has_children, expanded: true, target: Some(ent) },
                    action,
                )?;
                if ent == self.inspected {
                    self.ui.world.add_tag_typed::<Selected>(row_ent)?;
                }
                rows_added += 1;
            }

            if depth < 5 {
                let mut rev = children.len();
                while rev > 0 {
                    rev -= 1;
                    if let Some(&ch) = children.get(rev) {
                        stack.try_push((ch, depth.saturating_add(1))).map_err(|r| UiError::Alloc(r.why))?;
                    }
                }
            }
        }
        Ok(())
    }

    fn rebuild_inspector(&mut self) -> Result<(), UiError> {
        let alloc = self.ui.alloc();
        self.ui.clear_children(self.inspector_props)?;
        let target = self.inspected;
        if !self.ui.world.is_alive(target) {
            let _ = self.ui.spawn_label(self.inspector_props, "InspDead", "No live entity selected", true)?;
            return Ok(());
        }

        let mut name_s = OwnString::new_in(alloc);
        name_s.try_push_str(self.ui.world.entity_name(target).unwrap_or("Entity"))?;
        let kind_str = self
            .ui
            .world
            .get::<WidgetKind>(target)?
            .map_or("StylePrefab", |k| k.as_str());

        let header = self.ui.spawn_container(
            self.inspector_props,
            "InspHeadRow",
            WidgetKind::Container,
            LayoutStyle::row().width(Sizing::FILL).height(Sizing::Fixed(24.0)).align(Align::Center),
            None,
        )?;
        let _ = self.ui.spawn_label(header, "InspName", name_s.as_str(), false)?;
        let _ = self.ui.spawn_spacer(header)?;
        let _ = self.ui.spawn_badge(header, "InspKind", kind_str, BadgeTone::Fire)?;

        let mut buf = OwnString::new_in(alloc);
        let _ = write!(buf, "slot {} · gen {}", target.slot(), target.generation());
        let _ = self.ui.spawn_property_row(self.inspector_props, "InspId", "Entity", buf.as_str())?;

        // Parent (`ChildOf`) relation
        buf.clear();
        if let Some(parent) = self.ui.world.parent_of(target)? {
            let pn = self.ui.world.entity_name(parent).unwrap_or("Entity");
            let _ = write!(buf, "{pn} (#{})", parent.slot());
        } else {
            let _ = buf.try_push_str("none (root)");
        }
        let _ = self.ui.spawn_property_row(self.inspector_props, "InspChildOf", "ChildOf", buf.as_str())?;

        // Inherited `IsA` style prefab relation
        buf.clear();
        let isa_targets = self.ui.world.targets_of(target, self.ui.world.is_a())?;
        if let Some(&pf) = isa_targets.first() {
            let pn = self.ui.world.entity_name(pf).unwrap_or("Prefab");
            let _ = write!(buf, "{pn} (live cascade)");
        } else if self.ui.world.has_tag(target, self.ui.world.prefab())? {
            let _ = buf.try_push_str("Prefab (source)");
        } else {
            let _ = buf.try_push_str("local / none");
        }
        let _ = self.ui.spawn_property_row(self.inspector_props, "InspIsA", "IsA", buf.as_str())?;

        // DockedIn relation (if dock tab)
        let dock_rel = self.ui.world.relation_entity::<DockedIn>()?;
        let docked = self.ui.world.targets_of(target, dock_rel)?;
        if let Some(&leaf) = docked.first() {
            buf.clear();
            let ln = self.ui.world.entity_name(leaf).unwrap_or("DockLeaf");
            let _ = write!(buf, "{ln} (#{})", leaf.slot());
            let _ = self.ui.spawn_property_row(self.inspector_props, "InspDocked", "DockedIn", buf.as_str())?;
        }

        // Live ECS tags
        buf.clear();
        let tags: [(&str, bool); 5] = [
            ("Hovered", self.ui.world.has_tag_typed::<Hovered>(target)?),
            ("Selected", self.ui.world.has_tag_typed::<Selected>(target)?),
            ("Focused", self.ui.world.has_tag_typed::<Focused>(target)?),
            ("Interactive", self.ui.world.has_tag_typed::<Interactive>(target)?),
            ("Focusable", self.ui.world.has_tag_typed::<Focusable>(target)?),
        ];
        for (tname, present) in tags {
            if present {
                if !buf.is_empty() {
                    let _ = buf.try_push_str(", ");
                }
                let _ = buf.try_push_str(tname);
            }
        }
        if buf.is_empty() {
            let _ = buf.try_push_str("—");
        }
        let _ = self.ui.spawn_property_row(self.inspector_props, "InspTags", "Tags", buf.as_str())?;

        // Live ComputedRect
        if let Some(cr) = self.ui.world.get::<ComputedRect>(target)?.copied() {
            buf.clear();
            let _ = write!(
                buf,
                "{}×{} @ ({}, {})",
                f32_round_i32(cr.rect.w),
                f32_round_i32(cr.rect.h),
                f32_round_i32(cr.rect.x),
                f32_round_i32(cr.rect.y),
            );
            let _ = self.ui.spawn_property_row(self.inspector_props, "InspRect", "ComputedRect", buf.as_str())?;
            buf.clear();
            let _ = write!(buf, "clip[{}] · layer {}", cr.clip_id, cr.layer);
            let _ = self.ui.spawn_property_row(self.inspector_props, "InspClip", "GPU Clip", buf.as_str())?;
        }

        // Sync live override sliders to target's effective `PaintStyle`.
        let ps = self
            .ui
            .world
            .get_inherited::<PaintStyle>(target)?
            .copied()
            .unwrap_or_else(|| PaintStyle::panel(&self.ui.theme));
        self.ui.world.set_copy(
            self.insp_slider_radius,
            SliderState::new(ps.radius, 0.0, 18.0, 0.5, 1),
        )?;
        self.ui.world.set_copy(
            self.insp_slider_border,
            SliderState::new(ps.border_width, 0.0, 4.0, 0.25, 2),
        )?;
        Ok(())
    }

    /// Populate the hybrid `CanvasBatch` entity with:
    /// 1. Multi-lane Blaze Frame Telemetry Flamegraph (top 40%)
    /// 2. Interactive Relational ECS Scene Graph of the UI World (bottom 60%)
    fn populate_gpu_canvas(&mut self, time: f32) -> Result<(), UiError> {
        let alloc = self.ui.alloc();
        let bounds = self
            .ui
            .world
            .get::<ComputedRect>(self.canvas_ent)?
            .map_or(Rect::new(0.0, 0.0, 640.0, 560.0), |c| c.content_rect);
        let w = bounds.w.max(240.0);
        let h = bounds.h.max(220.0);
        let theme = self.ui.theme;

        let mut tmp = DrawList::new_in(alloc);
        tmp.begin_frame(w, h, 1.0, time)?;

        // --- Zone 1: Frame Telemetry Flamegraph (top 38%) ---
        let fg_h = (h * 0.36).clamp(112.0, 210.0);
        tmp.rounded_rect_bordered(
            Rect::new(8.0, 8.0, (w - 16.0).max(40.0), fg_h - 12.0),
            theme.radius_md,
            theme.bg,
            theme.line,
            1.0,
        )?;
        let _ = tmp.text(18.0, 16.0, "HYBRID GPU CANVAS · FRAME PIPELINE FLAMEGRAPH (SINGLE SDF DRAW)", 11.0, theme.ink3)?;

        let inner_x = 18.0;
        let inner_w = (w - 36.0).max(80.0);
        let lane_h = 20.0;
        let base_y = 36.0;

        // Deterministic microsecond-scale frame breakdown bars (pulsing slightly when animated)
        let pulse = if self.animate_graph { gpu::math::sin(time * 2.2) * 0.015 } else { 0.0 };
        let spans: [(f32, f32, u8, &str, Rgba); 9] = [
            (0.00, 0.98, 0, "frame_total (0.42 ms · 2380 FPS budget)", theme.bg4),
            (0.00, 0.14, 1, "input", theme.blue),
            (0.15, 0.45 + pulse, 1, "ecs.layout_2pass", theme.fire),
            (0.46 + pulse, 0.56, 1, "dock", theme.emerald),
            (0.57, 0.82, 1, "ecs.extract_gpu", theme.warn),
            (0.83, 0.98, 1, "webgpu.draw", theme.violet),
            (0.15, 0.29, 2, "measure", theme.fire_dim),
            (0.30, 0.45 + pulse, 2, "clips[]", theme.fire),
            (0.57, 0.82, 2, "sdf80 + glyph64", theme.warn),
        ];

        for (x0, x1, lane, label, col) in spans {
            let sx = inner_x + x0 * inner_w;
            let sw = ((x1 - x0) * inner_w).max(10.0);
            let sy = base_y + f32::from(lane) * (lane_h + 4.0);
            if sy + lane_h > fg_h - 6.0 {
                continue;
            }
            let r = Rect::new(sx, sy, sw, lane_h);
            tmp.rounded_rect_bordered(r, 3.5, col.with_alpha(0.28), col, 1.0)?;
            let _ = tmp.text_max_width(sx + 6.0, sy + 4.0, label, 10.5, theme.ink1, (sw - 10.0).max(0.0))?;
        }

        // --- Zone 2: Live Relational ECS Scene Graph of the UI World (bottom 62%) ---
        let g_top = fg_h + 4.0;
        let g_rect = Rect::new(8.0, g_top, (w - 16.0).max(40.0), (h - g_top - 8.0).max(80.0));
        tmp.rounded_rect_bordered(g_rect, theme.radius_md, theme.bg, theme.line, 1.0)?;

        // Subtle engineering dot grid
        let step = 28.0 * self.canvas_zoom;
        let mut gx = g_rect.x + 18.0;
        while gx < g_rect.right() - 8.0 {
            let mut gy = g_rect.y + 34.0;
            while gy < g_rect.bottom() - 8.0 {
                tmp.circle(gx, gy, 1.0, 0.0, theme.line)?;
                gy += step;
            }
            gx += step;
        }

        let _ = tmp.text_max_width(
            g_rect.x + 10.0,
            g_rect.y + 8.0,
            "LIVE UI ECS GRAPH · ChildOf (fire) · IsA (blue) · Inspects (violet) · Click node",
            10.5,
            theme.ink3,
            (g_rect.w - 20.0).max(40.0),
        )?;

        // Collect a curated set of live UI ECS entities (prefabs + dock + toolbar + inspector)
        self.graph_nodes.clear();
        let pf = self.ui.prefabs;
        let prefab_list = [pf.root, pf.panel, pf.card, pf.toolbar, pf.button, pf.input, pf.row];
        let cx = g_rect.center().x;
        let cy = g_rect.y + 28.0 + (g_rect.h - 36.0) * 0.54;
        let rx = (g_rect.w * 0.40 * self.canvas_zoom).min(g_rect.w * 0.44);
        let ry = (g_rect.h * 0.34 * self.canvas_zoom).min(g_rect.h * 0.38);

        // Row 1 (top arc): Style Prefabs (`IsA` targets)
        let pf_count = prefab_list.len();
        for (i, &ent) in prefab_list.iter().enumerate() {
            let frac = f32::from(u16::try_from(i).unwrap_or(0)) / f32::from(u16::try_from(pf_count - 1).unwrap_or(1));
            let nx = g_rect.x + 48.0 + frac * (g_rect.w - 96.0).max(20.0);
            let ny = g_rect.y + 52.0;
            self.graph_nodes.try_push((ent, nx, ny, 9.0)).map_err(|r| UiError::Alloc(r.why))?;
        }

        // Center hub: UiRoot + Dock root + Panes + Inspector
        let scene_list: [Entity; 7] = [
            self.ui.root,
            self.pane_hierarchy,
            self.pane_canvas,
            self.pane_shaders,
            self.pane_inspector,
            self.pane_prefabs,
            self.canvas_ent,
        ];
        let phase = if self.animate_graph { time * 0.22 } else { 0.0 };
        for (i, &ent) in scene_list.iter().enumerate() {
            if !self.ui.world.is_alive(ent) {
                continue;
            }
            let (nx, ny, nr) = if i == 0 {
                (cx, cy - ry * 0.22, 12.0)
            } else {
                let angle = f32::from(u16::try_from(i - 1).unwrap_or(0)) * (core::f32::consts::TAU / 6.0) + phase;
                (cx + gpu::math::cos(angle) * rx * 0.72, cy + 18.0 + gpu::math::sin(angle) * ry * 0.78, 10.0)
            };
            self.graph_nodes.try_push((ent, nx, ny, nr)).map_err(|r| UiError::Alloc(r.why))?;
        }

        // Draw live ECS relation edges between visible graph nodes
        let node_slice = self.graph_nodes.as_slice();
        let isa_rel = self.ui.world.is_a();
        let inspects_rel = self.ui.world.relation_entity::<Inspects>()?;

        for &(ent, x0, y0, _) in node_slice {
            if let Some(parent) = self.ui.world.parent_of(ent)? {
                if let Some(&(_, x1, y1, _)) = node_slice.iter().find(|&&(e, _, _, _)| e == parent) {
                    tmp.line(x0, y0, x1, y1, 1.5, theme.fire_dim)?;
                }
            }
            let isas = self.ui.world.targets_of(ent, isa_rel)?;
            for &pf_t in isas.as_slice() {
                if let Some(&(_, x1, y1, _)) = node_slice.iter().find(|&&(e, _, _, _)| e == pf_t) {
                    tmp.line(x0, y0, x1, y1, 1.2, theme.blue.with_alpha(0.55))?;
                }
            }
            let insps = self.ui.world.targets_of(ent, inspects_rel)?;
            for &it in insps.as_slice() {
                if let Some(&(_, x1, y1, _)) = node_slice.iter().find(|&&(e, _, _, _)| e == it) {
                    tmp.line(x0, y0, x1, y1, 2.0, theme.violet)?;
                }
            }
        }

        // Draw SDF graph nodes + selection rings + labels
        for &(ent, nx, ny, nr) in node_slice {
            let is_prefab = self.ui.world.has_tag(ent, self.ui.world.prefab())?;
            let is_insp = ent == self.inspected;
            let fill = if is_insp {
                theme.fire
            } else if is_prefab {
                theme.blue
            } else if ent == self.ui.root {
                theme.emerald
            } else {
                theme.bg4
            };
            if is_insp {
                tmp.circle(nx, ny, nr + 5.5, 1.8, theme.fire)?;
                tmp.circle(nx, ny, nr + 9.5, 1.0, theme.fire_dim)?;
            } else if is_prefab {
                tmp.circle(nx, ny, nr + 3.0, 1.0, theme.blue.with_alpha(0.5))?;
            }
            tmp.circle(nx, ny, nr, 0.0, fill)?;
            tmp.circle(nx, ny, nr, 1.2, theme.ink1.with_alpha(0.35))?;

            let name = self.ui.world.entity_name(ent).unwrap_or("Entity");
            let lw = tmp.measure_text(name, 10.0);
            let lx = nx - lw * 0.5;
            let ly = ny + nr + 4.0;
            tmp.rounded_rect(
                Rect::new(lx - 4.0, ly - 1.5, lw + 8.0, 14.0),
                3.0,
                theme.bg.with_alpha(0.86),
            )?;
            let _ = tmp.text(lx, ly, name, 10.0, if is_insp { theme.ink1 } else { theme.ink2 })?;
        }

        // Write packed primitives into the entity's `CanvasBatch` component!
        if let Some(batch) = self.ui.world.get_mut::<CanvasBatch<A>>(self.canvas_ent)? {
            batch.clear();
            batch.local_space = true;
            for &r in tmp.rects.as_slice() {
                batch.rects.try_push(r).map_err(|err| UiError::Alloc(err.why))?;
            }
            for &g in tmp.glyphs.as_slice() {
                batch.glyphs.try_push(g).map_err(|err| UiError::Alloc(err.why))?;
            }
        }
        Ok(())
    }
}

fn f32_round_i32(v: f32) -> i32 {
    let neg = v < 0.0;
    let abs_v = gpu::math::round(if neg { -v } else { v });
    if !abs_v.is_finite() || abs_v >= 1_000_000.0 {
        return 0;
    }
    let mut lo: u32 = 0;
    let mut hi: u32 = 1_000_000;
    while lo < hi {
        let mid = lo + (hi - lo + 1) / 2;
        let mf = f32::from(u16::try_from(mid >> 16).unwrap_or(0)) * 65536.0
            + f32::from(u16::try_from(mid & 0xFFFF).unwrap_or(0));
        if mf <= abs_v {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let i = i32::try_from(lo).unwrap_or(0);
    if neg { -i } else { i }
}
