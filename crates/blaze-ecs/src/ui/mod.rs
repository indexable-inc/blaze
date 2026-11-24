//! Pure `#![no_std]`, `#![forbid(unsafe_code)]`, `#![blaze::sans_io]` Relational ECS Explorer
//! UI and WebGPU Scene Engine built directly on [`crate::World`].
//!
//! Designed to run inside `wasm32-unknown-unknown` (`blaze-ecs-wasm`) with a minimal JS/WebGPU
//! host shim:
//! - Mirrors the remote ECS snapshot into a client-side relational [`World`] (`RemoteSlot`,
//!   `GraphNode`, `SpatialBody`, and real `ChildOf` / `IsA` pairs).
//! - Renders the entire compact, Phosphor-light-icon UI as HTML directly from Rust/WASM.
//! - Runs deterministic force-directed graph physics & live `Transform`/`Velocity` spatial
//!   projection in Rust/WASM, writing packed `f32` instance buffers directly for zero-copy
//!   WebGPU upload.

pub mod graph;
pub mod icons;

use core::alloc::Allocator;
use core::fmt::Write as _;

use blaze_data::{Map, Value};
use blaze_int::Int as _;
use blaze_json::Limits as JsonLimits;
use blaze_own::{String as OwnString, Vec as OwnVec};

use crate::{Builtins, World};

use self::graph::{GraphBuffers, GraphMode, GraphNode, NodeEntry, RemoteSlot, SpatialBody};
use self::icons::{BLAZE_LOGO_SVG, Icon, schema_icon, write_icon};

/// Active top-level tab in the Explorer pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewKind {
    Inspector,
    Graph,
    Archetypes,
    Query,
    Schemas,
}

impl ViewKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inspector => "inspector",
            Self::Graph => "graph",
            Self::Archetypes => "archetypes",
            Self::Query => "query",
            Self::Schemas => "schemas",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "inspector" => Some(Self::Inspector),
            "graph" => Some(Self::Graph),
            "archetypes" => Some(Self::Archetypes),
            "query" => Some(Self::Query),
            "schemas" => Some(Self::Schemas),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MatchKind {
    Entity,
    Tag,
    Comp,
    Field,
}

struct FindMatch<A: Allocator + Copy> {
    slot: u32,
    kind: MatchKind,
    comp: OwnString<A>,
    field: OwnString<A>,
}

/// Complete client-side Relational ECS Explorer application state.
pub struct ExplorerApp<A: Allocator + Copy + 'static> {
    alloc: A,
    /// Client-side relational ECS mirroring entities, `ChildOf`/`IsA` pairs, and WebGPU scene state.
    pub world: World<A>,
    entries: OwnVec<NodeEntry<A>, A>,
    snapshot: Option<Value<A>>,
    selected_slot: Option<u32>,
    active_view: ViewKind,
    graph_mode: GraphMode,
    hide_builtins: bool,
    light_theme: bool,
    collapsed_tree: OwnVec<u32, A>,
    collapsed_comps: OwnVec<OwnString<A>, A>,
    find_open: bool,
    find_query: OwnString<A>,
    find_matches: OwnVec<FindMatch<A>, A>,
    find_cur: Option<usize>,
    hovered_slot: Option<u32>,
    query_with: OwnString<A>,
    query_without: OwnString<A>,
    query_result: Option<Value<A>>,
    /// Packed WebGPU buffers updated by [`Self::step_graph`].
    pub gpu: GraphBuffers<A>,
    out_buf: OwnString<A>,
}

impl<A: Allocator + Copy + 'static> ExplorerApp<A> {
    /// Construct a fresh Explorer app backed by an explicit allocator and client [`World`].
    pub fn new_in(alloc: A) -> Result<Self, crate::EcsError> {
        let mut world = World::new_in(alloc)?;
        let _ = world.register_copy::<RemoteSlot>()?;
        let _ = world.register_copy::<GraphNode>()?;
        let _ = world.register_copy::<SpatialBody>()?;
        Ok(Self {
            alloc,
            world,
            entries: OwnVec::new_in(alloc),
            snapshot: None,
            selected_slot: None,
            active_view: ViewKind::Inspector,
            graph_mode: GraphMode::Relational,
            hide_builtins: true,
            light_theme: false,
            collapsed_tree: OwnVec::new_in(alloc),
            collapsed_comps: OwnVec::new_in(alloc),
            find_open: false,
            find_query: OwnString::new_in(alloc),
            find_matches: OwnVec::new_in(alloc),
            find_cur: None,
            hovered_slot: None,
            query_with: OwnString::new_in(alloc),
            query_without: OwnString::new_in(alloc),
            query_result: None,
            gpu: GraphBuffers::new_in(alloc),
            out_buf: OwnString::new_in(alloc),
        })
    }

    /// Number of mirrored entities currently live in the client-side relational [`World`].
    pub fn mirrored_entity_count(&self) -> usize {
        self.entries.len()
    }

    /// Current active view.
    pub fn active_view(&self) -> ViewKind {
        self.active_view
    }

    /// Current selected server entity slot.
    pub fn selected_slot(&self) -> Option<u32> {
        self.selected_slot
    }

    /// Load a `/api/world` JSON snapshot into the client-side relational [`World`].
    pub fn load_world_json(&mut self, bytes: &[u8]) -> bool {
        let Ok(val) = blaze_json::read_in(bytes, JsonLimits::SHIPPED, self.alloc) else {
            return false;
        };
        if !matches!(val, Value::Map(_)) {
            return false;
        }
        self.snapshot = Some(val);
        self.sync_client_world();
        self.ensure_selection();
        if self.find_open {
            self.recompute_find(true);
        }
        true
    }

    /// Apply a JSON UI action from the thin JS host shim.
    pub fn apply_action_json(&mut self, bytes: &[u8]) -> bool {
        let Ok(val) = blaze_json::read_in(bytes, JsonLimits::SHIPPED, self.alloc) else {
            return false;
        };
        let Some(map) = val.as_map() else { return false };
        let Some(op) = map_str(map, "op") else { return false };
        match op {
            "select" => {
                if let Some(slot) = map_u32(map, "slot") {
                    self.selected_slot = Some(slot);
                    if map_bool(map, "openInspector").unwrap_or(false) {
                        self.active_view = ViewKind::Inspector;
                    }
                    self.expand_ancestors(slot);
                    true
                } else {
                    false
                }
            }
            "tab" => {
                if let Some(v) = map_str(map, "view").and_then(ViewKind::from_str) {
                    self.active_view = v;
                    true
                } else {
                    false
                }
            }
            "graph_mode" => {
                match map_str(map, "mode") {
                    Some("spatial") => {
                        self.graph_mode = GraphMode::Spatial;
                        true
                    }
                    Some("relational") => {
                        self.graph_mode = GraphMode::Relational;
                        true
                    }
                    _ => false,
                }
            }
            "toggle_tree" => {
                if let Some(slot) = map_u32(map, "slot") {
                    if let Some(pos) = self.collapsed_tree.iter().position(|&s| s == slot) {
                        let _ = self.collapsed_tree.remove_swap(pos);
                    } else {
                        let _ = self.collapsed_tree.try_push(slot);
                    }
                    true
                } else {
                    false
                }
            }
            "toggle_comp" => {
                if let Some(name) = map_str(map, "name") {
                    let want_open = map_bool(map, "open");
                    let pos = self.collapsed_comps.iter().position(|s| s.as_str() == name);
                    match (want_open, pos) {
                        (Some(true), Some(i)) => { let _ = self.collapsed_comps.remove_swap(i); }
                        (Some(false), None) => {
                            if let Ok(s) = OwnString::try_from_str_in(name, self.alloc) {
                                let _ = self.collapsed_comps.try_push(s);
                            }
                        }
                        (None, Some(i)) => { let _ = self.collapsed_comps.remove_swap(i); }
                        (None, None) => {
                            if let Ok(s) = OwnString::try_from_str_in(name, self.alloc) {
                                let _ = self.collapsed_comps.try_push(s);
                            }
                        }
                        _ => {}
                    }
                    true
                } else {
                    false
                }
            }
            "toggle_all_comps" => {
                self.toggle_all_selected_comps();
                true
            }
            "builtins" => {
                self.hide_builtins = !self.hide_builtins;
                self.sync_client_world();
                if self.find_open {
                    self.recompute_find(true);
                }
                true
            }
            "theme" => {
                if let Some(light) = map_bool(map, "light") {
                    self.light_theme = light;
                } else {
                    self.light_theme = !self.light_theme;
                }
                true
            }
            "find_open" => {
                self.find_open = true;
                self.recompute_find(true);
                true
            }
            "find_close" => {
                self.find_open = false;
                true
            }
            "find_query" => {
                if let Some(q) = map_str(map, "q") {
                    self.find_query.clear();
                    let _ = self.find_query.try_push_str(q);
                    self.recompute_find(false);
                    self.reveal_current_match();
                    true
                } else {
                    false
                }
            }
            "find_step" => {
                let delta = map_i64(map, "delta").unwrap_or(1);
                self.step_find(delta);
                true
            }
            "query_result" => {
                if let Some(w) = map_str(map, "with") {
                    self.query_with.clear();
                    let _ = self.query_with.try_push_str(w);
                }
                if let Some(wo) = map_str(map, "without") {
                    self.query_without.clear();
                    let _ = self.query_without.try_push_str(wo);
                }
                if let Some(raw) = map_str(map, "raw") {
                    if let Ok(parsed) = blaze_json::read_in(raw.as_bytes(), JsonLimits::SHIPPED, self.alloc) {
                        self.query_result = Some(parsed);
                    }
                }
                true
            }
            "patch_local" => {
                // Live-update SpatialBody in the client ECS immediately during drag-scrubbing!
                let Some(slot) = map_u32(map, "slot") else { return false };
                let Some(comp) = map_str(map, "comp") else { return false };
                let Some(field) = map_str(map, "field") else { return false };
                let num = map_f64(map, "num");
                let flag = map_bool(map, "bool");
                self.patch_local_spatial(slot, comp, field, num, flag);
                true
            }
            _ => false,
        }
    }

    fn patch_local_spatial(
        &mut self,
        slot: u32,
        comp: &str,
        field: &str,
        num: Option<f64>,
        flag: Option<bool>,
    ) {
        let Some(entry) = self.entries.iter().find(|e| e.slot == slot) else { return };
        let ent = entry.entity;
        if let Ok(Some(sb)) = self.world.get_mut::<SpatialBody>(ent) {
            if comp == "Transform" {
                if let Some(v) = num {
                    let f = graph::f64_to_f32(v);
                    match field {
                        "x" => sb.x = f,
                        "y" => sb.y = f,
                        "z" => sb.z = f,
                        _ => {}
                    }
                }
                if field == "active" {
                    if let Some(b) = flag {
                        sb.active = b;
                    }
                }
            } else if comp == "Velocity" {
                if let Some(v) = num {
                    let f = graph::f64_to_f32(v);
                    match field {
                        "dx" => sb.dx = f,
                        "dy" => sb.dy = f,
                        "dz" => sb.dz = f,
                        _ => {}
                    }
                }
            }
        }
    }

    /// Sync the parsed remote JSON snapshot into our real client-side relational [`World`],
    /// preserving existing force-graph positions so live refreshes stay rock-steady.
    fn sync_client_world(&mut self) {
        // Save prior graph positions by slot so nodes don't jump on refresh.
        let mut prev_pos: OwnVec<(u32, f32, f32, f32, f32, bool), A> = OwnVec::new_in(self.alloc);
        for e in self.entries.as_slice() {
            if let Ok(Some(gb)) = self.world.get::<GraphNode>(e.entity) {
                let _ = prev_pos.try_push((e.slot, gb.x, gb.y, gb.vx, gb.vy, gb.pinned));
            }
        }

        // Rebuild the client-side relational ECS world cleanly.
        let Ok(mut new_world) = World::new_in(self.alloc) else { return };
        if new_world.register_copy::<RemoteSlot>().is_err()
            || new_world.register_copy::<GraphNode>().is_err()
            || new_world.register_copy::<SpatialBody>().is_err()
        {
            return;
        }
        let mut new_entries: OwnVec<NodeEntry<A>, A> = OwnVec::new_in(self.alloc);

        if let Some(ents) = self.snapshot_entities() {
            let mut vis_idx = 0usize;
            for ev in ents {
                let Some(em) = ev.as_map() else { continue };
                let Some(slot) = map_u32(em, "slot") else { continue };
                let name_str = map_str(em, "name").unwrap_or("");
                if self.hide_builtins && Builtins::is_builtin_name(name_str) {
                    continue;
                }
                let is_prefab = map_bool(em, "is_prefab").unwrap_or(false);
                let comp_count_usize = em.get("components").and_then(Value::as_map).map_or(0, Map::len);
                let comp_count = comp_count_usize.min(65535).truncate_to::<u16>();

                let parent_slot = em
                    .get("parents")
                    .and_then(Value::as_list)
                    .and_then(|l| l.first())
                    .and_then(val_u32);
                let prefab_slot = em
                    .get("prefabs")
                    .and_then(Value::as_list)
                    .and_then(|l| l.first())
                    .and_then(val_u32);

                let Ok(ent) = new_world.spawn() else { continue };
                let (x, y, vx, vy, pinned) = prev_pos
                    .iter()
                    .find(|p| p.0 == slot)
                    .map(|p| (p.1, p.2, p.3, p.4, p.5))
                    .unwrap_or_else(|| {
                        let (sx, sy) = graph::initial_spiral_pos(vis_idx);
                        (sx, sy, 0.0, 0.0, false)
                    });
                vis_idx += 1;

                let radius = if is_prefab {
                    9.5
                } else {
                    6.5 + f32::from(comp_count.min(6)) * 1.15
                };
                let _ = new_world.set_copy(ent, RemoteSlot(slot));
                let _ = new_world.set_copy(
                    ent,
                    GraphNode {
                        x,
                        y,
                        vx,
                        vy,
                        radius,
                        pinned,
                        is_prefab,
                        comp_count,
                    },
                );
                let spatial = extract_spatial(em);
                let _ = new_world.set_copy(ent, spatial);

                let mut label = OwnString::new_in(self.alloc);
                if name_str.is_empty() {
                    let _ = write!(label, "entity_{}", slot);
                } else {
                    let _ = label.try_push_str(name_str);
                }
                let _ = new_entries.try_push(NodeEntry {
                    entity: ent,
                    slot,
                    name: label,
                    parent_slot,
                    prefab_slot,
                });
            }

            // Wire real relational ECS `(ChildOf, parent)` and `(IsA, prefab)` pairs in `new_world`!
            let child_of = new_world.builtins().child_of;
            let is_a = new_world.builtins().is_a;
            for entry in new_entries.as_slice() {
                if let Some(ps) = entry.parent_slot {
                    if let Some(pe) = new_entries.iter().find(|x| x.slot == ps) {
                        let _ = new_world.add_pair(entry.entity, child_of, pe.entity);
                    }
                }
                if let Some(is_s) = entry.prefab_slot {
                    if let Some(pe) = new_entries.iter().find(|x| x.slot == is_s) {
                        let _ = new_world.add_pair(entry.entity, is_a, pe.entity);
                    }
                }
            }
        }

        self.world = new_world;
        self.entries = new_entries;
    }

    /// Advance client-side relational ECS physics & pack WebGPU buffers.
    pub fn step_graph(&mut self, dt: f32) {
        let mut hit_slots = OwnVec::new_in(self.alloc);
        let mut cur_slot = None;
        if self.find_open {
            for m in self.find_matches.as_slice() {
                if !hit_slots.iter().any(|&s| s == m.slot) {
                    let _ = hit_slots.try_push(m.slot);
                }
            }
            cur_slot = self.find_cur.and_then(|i| self.find_matches.get(i)).map(|m| m.slot);
        }
        graph::step_and_pack(
            &mut self.world,
            self.entries.as_slice(),
            self.graph_mode,
            dt,
            self.selected_slot,
            self.hovered_slot,
            hit_slots.as_slice(),
            cur_slot,
            self.light_theme,
            &mut self.gpu,
            self.alloc,
        );
    }

    /// Hit-test a WebGPU graph node in world space; returns slot or `-1`.
    pub fn pick_graph(&self, wx: f32, wy: f32, pad: f32) -> i32 {
        match graph::pick_node(&self.world, self.entries.as_slice(), self.graph_mode, wx, wy, pad) {
            Some(s) => s.narrow::<i32>().unwrap_or(-1),
            None => -1,
        }
    }

    /// Pin/move a node during pointer drag in the WebGPU graph.
    pub fn drag_graph(&mut self, slot: u32, wx: f32, wy: f32, active: bool) {
        if let Some(e) = self.entries.iter().find(|x| x.slot == slot) {
            if let Ok(Some(gb)) = self.world.get_mut::<GraphNode>(e.entity) {
                gb.pinned = active;
                if active {
                    gb.x = wx;
                    gb.y = wy;
                    gb.vx = 0.0;
                    gb.vy = 0.0;
                }
            }
        }
    }

    /// Set currently hovered WebGPU graph node slot (`< 0` clears hover).
    pub fn hover_graph(&mut self, slot: i32) {
        self.hovered_slot = if slot < 0 {
            None
        } else {
            slot.narrow::<u32>().ok()
        };
    }

    /// Return JSON `[cx, cy, span_w, span_h]` to fit all active WebGPU nodes cleanly.
    pub fn fit_graph_json(&mut self) -> &str {
        let (cx, cy, w, h) = graph::fit_bounds(&self.world, self.entries.as_slice(), self.graph_mode);
        self.out_buf.clear();
        let _ = write!(self.out_buf, "[{:.2},{:.2},{:.2},{:.2}]", cx, cy, w, h);
        self.out_buf.as_str()
    }

    /// Return JSON status for the `.page-find` bar and topbar counts.
    pub fn render_status_json(&mut self) -> &str {
        let arch_cnt = self.snapshot_stat("archetypes");
        let comp_cnt = self.snapshot_stat("components");
        let ent_cnt = self.snapshot_stat("entities");
        let has_builtins = self
            .snapshot_entities()
            .is_some_and(|es| es.iter().any(|v| v.as_map().and_then(|m| map_str(m, "name")).is_some_and(Builtins::is_builtin_name)));
        let n = self.find_matches.len();
        let cur_idx = self.find_cur.map_or(0, |i| i + 1);
        let q_empty = self.find_query.as_str().trim().is_empty();
        self.out_buf.clear();
        let _ = write!(
            self.out_buf,
            r#"{{"view":"{}","graphMode":"{}","selected":{},"entities":{},"archetypes":{},"components":{},"hasBuiltins":{},"showBuiltins":{},"findOpen":{},"findTotal":{},"findCur":{},"findNone":{}}}"#,
            self.active_view.as_str(),
            if self.graph_mode == GraphMode::Spatial { "spatial" } else { "relational" },
            self.selected_slot.map_or(-1i64, i64::from),
            ent_cnt,
            arch_cnt,
            comp_cnt,
            has_builtins,
            !self.hide_builtins,
            self.find_open,
            n,
            cur_idx,
            !q_empty && n == 0,
        );
        self.out_buf.as_str()
    }

    /// Render the complete compact left sidebar HTML (Hierarchy + Prefabs + icon-labeled footer).
    pub fn render_sidebar(&mut self) -> &str {
        self.out_buf.clear();
        let alloc = self.alloc;
        let mut out = OwnString::new_in(alloc);

        // Top brand bar with actual Blaze pixel-art flame logo + Spawn icon button
        let _ = out.try_push_str(r##"<div class="sidetop"><a class="brand" href="#" data-act="home" title="Blaze ECS Explorer">"##);
        let _ = out.try_push_str(BLAZE_LOGO_SVG);
        let _ = out.try_push_str(r#"<span>blaze</span><span class="sub">ecs</span></a><button class="iconbtn" id="btnSpawn" type="button" title="Spawn entity">"#);
        write_icon(&mut out, Icon::Plus, 14, "");
        let _ = out.try_push_str(r#"</button></div>"#);

        // Compact Find button
        let _ = out.try_push_str(r#"<button class="searchbtn" id="btnOpenFind" type="button">"#);
        write_icon(&mut out, Icon::Search, 13, "");
        let _ = out.try_push_str(r#"<span>Find…</span><kbd>⌘F</kbd></button>"#);

        // Scrollable navlist with glide-hov + Hierarchy + Prefabs
        let _ = out.try_push_str(r#"<div class="scrollbox"><div class="navlist" id="navList"><div class="glide-hov" id="glideHov"></div>"#);

        let vis_count = self.entries.len();
        let _ = out.try_push_str(r#"<details class="sec" open><summary class="sechead">"#);
        write_icon(&mut out, Icon::Caret, 11, " chev");
        write_icon(&mut out, Icon::Tree, 12, " sec-ico");
        let _ = write!(out, r#"<span>Hierarchy</span><span class="cnt">{}</span></summary><div id="tree">"#, vis_count);

        self.write_hierarchy_tree(&mut out);

        let _ = out.try_push_str(r#"</div></details>"#);

        // Prefabs section
        let prefab_count = self.snapshot_prefabs().map_or(0, |p| p.len());
        let _ = out.try_push_str(r#"<details class="sec" open><summary class="sechead">"#);
        write_icon(&mut out, Icon::Caret, 11, " chev");
        write_icon(&mut out, Icon::Diamond, 12, " sec-ico");
        let _ = write!(out, r#"<span>Prefabs</span><span class="cnt">{}</span></summary><div id="prefabs">"#, prefab_count);

        if let Some(prefabs) = self.snapshot_prefabs() {
            if prefabs.is_empty() {
                let _ = out.try_push_str(r#"<div class="empty-sm">None</div>"#);
            } else {
                for pv in prefabs {
                    let Some(pm) = pv.as_map() else { continue };
                    let Some(slot) = map_u32(pm, "slot") else { continue };
                    let is_sel = self.selected_slot == Some(slot);
                    let _ = write!(
                        out,
                        r#"<div class="row" data-slot="{}"{}>"#,
                        slot,
                        if is_sel { r#" aria-current="page""# } else { "" }
                    );
                    write_icon(&mut out, Icon::Diamond, 12, " row-ico fire");
                    let _ = out.try_push_str(r#"<span class="label">"#);
                    write_entity_label_escaped(&mut out, pm, slot);
                    let _ = write!(
                        out,
                        r#"</span><button class="inst" data-inst="{}" type="button" title="Instantiate from prefab">"#,
                        slot
                    );
                    write_icon(&mut out, Icon::Plus, 11, "");
                    let _ = out.try_push_str(r#"</button></div>"#);
                }
            }
        }
        let _ = out.try_push_str(r#"</div></details></div></div>"#);

        // Icon-labeled footer (zero clipped text!)
        let ent_cnt = self.snapshot_stat("entities");
        let arch_cnt = self.snapshot_stat("archetypes");
        let _ = write!(
            out,
            r#"<div class="foot"><div class="foot-stats" title="{} entities · {} archetype tables"><span class="fstat">"#,
            ent_cnt, arch_cnt
        );
        write_icon(&mut out, Icon::Cube, 12, "");
        let _ = write!(out, r#"{}</span><span class="fstat">"#, ent_cnt);
        write_icon(&mut out, Icon::Table, 12, "");
        let _ = write!(out, r#"{}</span></div><div class="foot-acts">"#, arch_cnt);
        let _ = out.try_push_str(r#"<button class="iconbtn sm" id="btnJson" type="button" title="Export world JSON">"#);
        write_icon(&mut out, Icon::Braces, 13, "");
        let _ = out.try_push_str(r#"</button><button class="iconbtn sm" id="btnTab" type="button" title="Export world TAB">"#);
        write_icon(&mut out, Icon::Rows, 13, "");
        let _ = out.try_push_str(r#"</button><button class="iconbtn sm" id="btnTheme" type="button" title="Toggle light / dark">"#);
        write_icon(&mut out, Icon::Contrast, 13, "");
        let _ = out.try_push_str(r#"</button></div></div>"#);

        self.out_buf = out;
        self.out_buf.as_str()
    }

    /// Iterative (zero-recursion) hierarchy tree renderer using an explicit stack.
    fn write_hierarchy_tree(&self, out: &mut OwnString<A>) {
        let Some(ents) = self.snapshot_entities() else {
            let _ = out.try_push_str(r#"<div class="empty-sm">Loading…</div>"#);
            return;
        };
        let mut vis_slots: OwnVec<u32, A> = OwnVec::new_in(self.alloc);
        for e in self.entries.as_slice() {
            let _ = vis_slots.try_push(e.slot);
        }
        if vis_slots.is_empty() {
            let _ = out.try_push_str(r#"<div class="empty-sm">No entities</div>"#);
            return;
        }

        // Collect root slots (no visible parent)
        let mut stack: OwnVec<(u32, bool), A> = OwnVec::new_in(self.alloc);
        let mut roots: OwnVec<u32, A> = OwnVec::new_in(self.alloc);
        for ev in ents {
            let Some(em) = ev.as_map() else { continue };
            let Some(slot) = map_u32(em, "slot") else { continue };
            if !vis_slots.iter().any(|&s| s == slot) {
                continue;
            }
            let has_vis_parent = em
                .get("parents")
                .and_then(Value::as_list)
                .is_some_and(|ps| ps.iter().filter_map(val_u32).any(|p| vis_slots.iter().any(|&s| s == p)));
            if !has_vis_parent {
                let _ = roots.try_push(slot);
            }
        }
        // Push roots in reverse so stack pops in order
        for &rslot in roots.iter().rev() {
            let _ = stack.try_push((rslot, false));
        }

        let cur_match_slot = if self.find_open {
            self.find_cur.and_then(|i| self.find_matches.get(i)).map(|m| m.slot)
        } else {
            None
        };

        let mut depth_guard = 0usize;
        while let Some((slot, is_close)) = stack.pop() {
            if is_close {
                let _ = out.try_push_str("</div>");
                continue;
            }
            depth_guard += 1;
            if depth_guard > 4096 {
                break;
            }
            let Some(em) = self.entity_map_by_slot(slot) else { continue };
            let is_prefab = map_bool(em, "is_prefab").unwrap_or(false);
            let is_sel = self.selected_slot == Some(slot);
            let is_shut = self.collapsed_tree.iter().any(|&s| s == slot);
            let is_hit = self.find_open && self.find_matches.iter().any(|m| m.slot == slot);
            let is_cur = cur_match_slot == Some(slot);

            let mut kids: OwnVec<u32, A> = OwnVec::new_in(self.alloc);
            if let Some(cl) = em.get("children").and_then(Value::as_list) {
                for cv in cl {
                    if let Some(cs) = val_u32(cv) {
                        if vis_slots.iter().any(|&s| s == cs) {
                            let _ = kids.try_push(cs);
                        }
                    }
                }
            }
            let has_kids = !kids.is_empty();

            let _ = out.try_push_str(r#"<div class="row"#);
            if is_hit {
                let _ = out.try_push_str(" find-hit");
            }
            if is_cur {
                let _ = out.try_push_str(" find-cur");
            }
            let _ = write!(
                out,
                r#"" data-slot="{}"{}>"#,
                slot,
                if is_sel { r#" aria-current="page""# } else { "" }
            );

            // Caret twisty
            let _ = write!(
                out,
                r#"<span class="twisty{}" data-toggle="{}">"#,
                if !has_kids {
                    " leaf"
                } else if is_shut {
                    " shut"
                } else {
                    ""
                },
                slot
            );
            write_icon(out, Icon::Caret, 10, "");
            let _ = out.try_push_str("</span>");

            // Semantic node icon (Diamond for prefab, Tree for group, Cube for entity)
            if is_prefab {
                write_icon(out, Icon::Diamond, 12, " row-ico fire");
            } else if has_kids {
                write_icon(out, Icon::Layers, 12, " row-ico");
            } else {
                write_icon(out, Icon::Cube, 12, " row-ico");
            }

            let _ = out.try_push_str(r#"<span class="label">"#);
            let name = map_str(em, "name").unwrap_or("");
            if name.is_empty() {
                let mut fb = OwnString::new_in(self.alloc);
                let _ = write!(fb, "entity_{}", slot);
                self.write_highlighted(out, fb.as_str(), is_cur);
            } else {
                self.write_highlighted(out, name, is_cur);
            }
            let _ = write!(out, r#"</span><span class="num">{}</span></div>"#, slot);

            if has_kids && !is_shut {
                let _ = out.try_push_str(r#"<div class="kids">"#);
                let _ = stack.try_push((slot, true));
                for &kslot in kids.iter().rev() {
                    let _ = stack.try_push((kslot, false));
                }
            }
        }
    }

    /// Render the active main view HTML (`Inspector` / `Graph` / `Archetypes` / `Query` / `Schemas`).
    pub fn render_main(&mut self) -> &str {
        self.out_buf.clear();
        let mut out = OwnString::new_in(self.alloc);
        match self.active_view {
            ViewKind::Inspector => self.write_inspector(&mut out),
            ViewKind::Graph => self.write_graph_view(&mut out),
            ViewKind::Archetypes => self.write_archetypes(&mut out),
            ViewKind::Query => self.write_query(&mut out),
            ViewKind::Schemas => self.write_schemas(&mut out),
        }
        self.out_buf = out;
        self.out_buf.as_str()
    }

    fn write_inspector(&self, out: &mut OwnString<A>) {
        let Some(slot) = self.selected_slot else {
            let _ = out.try_push_str(r#"<div class="empty">Select an entity from the hierarchy to inspect and live-edit its components.</div>"#);
            return;
        };
        let Some(em) = self.entity_map_by_slot(slot) else {
            let _ = out.try_push_str(r#"<div class="empty">Select an entity from the hierarchy.</div>"#);
            return;
        };

        let generation = map_u32(em, "gen").unwrap_or(0);
        let table_id = map_u32(em, "table_id").unwrap_or(0);
        let is_prefab = map_bool(em, "is_prefab").unwrap_or(false);
        let cur_match = if self.find_open {
            self.find_cur.and_then(|i| self.find_matches.get(i))
        } else {
            None
        };
        let is_ent_cur = cur_match.is_some_and(|m| m.slot == slot && m.kind == MatchKind::Entity);

        // Breadcrumb chain (iterative, omitted for root entities)
        let mut chain: OwnVec<u32, A> = OwnVec::new_in(self.alloc);
        let mut walk = Some(slot);
        let mut guard = 0usize;
        while let Some(ws) = walk {
            if guard >= 16 {
                break;
            }
            guard += 1;
            let _ = chain.try_push(ws);
            walk = self
                .entity_map_by_slot(ws)
                .and_then(|m| m.get("parents"))
                .and_then(Value::as_list)
                .and_then(|l| l.first())
                .and_then(val_u32);
        }

        let _ = out.try_push_str(r#"<div class="ehead">"#);
        if chain.len() > 1 {
            let _ = out.try_push_str(r#"<div class="crumb">"#);
            let mut first_crumb = true;
            for &ps in chain.iter().skip(1).rev() {
                if !first_crumb {
                    let _ = out.try_push_str(r#"<span class="sep">/</span>"#);
                }
                first_crumb = false;
                let _ = write!(out, r#"<a data-jump="{}">"#, ps);
                if let Some(pm) = self.entity_map_by_slot(ps) {
                    write_entity_label_escaped(out, pm, ps);
                } else {
                    let _ = write!(out, "{}", ps);
                }
                let _ = out.try_push_str("</a>");
            }
            let _ = out.try_push_str(r#"<span class="sep">/</span></div>"#);
        }

        // Main title row with icon-labeled stats (NO verbose "slot", "gen", "tbl" text!)
        let _ = out.try_push_str(r#"<div class="ehead-main"><div class="ehead-left">"#);
        if is_prefab {
            write_icon(out, Icon::Diamond, 17, " title-ico fire");
        } else {
            write_icon(out, Icon::Cube, 17, " title-ico");
        }
        let _ = out.try_push_str("<h1>");
        let name = map_str(em, "name").unwrap_or("");
        if name.is_empty() {
            let mut fb = OwnString::new_in(self.alloc);
            let _ = write!(fb, "entity_{}", slot);
            self.write_highlighted(out, fb.as_str(), is_ent_cur);
        } else {
            self.write_highlighted(out, name, is_ent_cur);
        }
        let _ = out.try_push_str("</h1>");

        // Crisp icon stat pills: [Slot] 9  [Gen] 1  [Table] 31
        let _ = write!(out, r#"<div class="stat-pills"><span class="spill" title="Entity slot {}">"#, slot);
        write_icon(out, Icon::Slot, 12, "");
        let _ = write!(out, r#"{}</span><span class="spill" title="Generation {}">"#, slot, generation);
        write_icon(out, Icon::Gen, 12, "");
        let _ = write!(out, r#"{}</span><span class="spill" title="Archetype table {}">"#, generation, table_id);
        write_icon(out, Icon::Table, 12, "");
        let _ = write!(out, r#"{}</span>"#, table_id);

        if is_prefab {
            let _ = out.try_push_str(r#"<span class="isa-chip" title="Prefab archetype">"#);
            write_icon(out, Icon::Diamond, 11, "");
            let _ = out.try_push_str("Prefab</span>");
        }
        if let Some(prefabs) = em.get("prefabs").and_then(Value::as_list) {
            for pv in prefabs {
                if let Some(ps) = val_u32(pv) {
                    let _ = write!(out, r#"<a class="isa-chip" data-jump="{}" title="Inherits prefab (IsA)">"#, ps);
                    write_icon(out, Icon::Link, 11, "");
                    if let Some(pm) = self.entity_map_by_slot(ps) {
                        write_entity_label_escaped(out, pm, ps);
                    } else {
                        let _ = write!(out, "{}", ps);
                    }
                    let _ = out.try_push_str("</a>");
                }
            }
        }
        let _ = out.try_push_str("</div></div>");

        // Header right actions: open in WebGPU Graph + Delete icon button
        let _ = out.try_push_str(r#"<div class="ehead-acts"><button class="iconbtn" data-open-graph="1" type="button" title="Focus in WebGPU Graph">"#);
        write_icon(out, Icon::Graph, 14, "");
        let _ = out.try_push_str(r#"</button><button class="iconbtn danger" id="btnDeleteEnt" type="button" title="Delete entity">"#);
        write_icon(out, Icon::Trash, 14, "");
        let _ = out.try_push_str(r#"</button></div></div>"#);

        // Compact icon-labeled properties strip: [Parent] select  +  [Tag] flat chips (NO `#`!) + borderless `+ tag`
        let cur_parent = em
            .get("parents")
            .and_then(Value::as_list)
            .and_then(|l| l.first())
            .and_then(val_u32);

        let _ = out.try_push_str(r#"<div class="props-bar"><div class="prop-group" title="Parent entity (ChildOf)">"#);
        write_icon(out, Icon::Parent, 13, " prop-ico");
        let _ = out.try_push_str(r#"<select class="flat-select" id="selParent" aria-label="Parent entity"><option value="">Root</option>"#);
        if let Some(ents) = self.snapshot_entities() {
            for ev in ents {
                let Some(om) = ev.as_map() else { continue };
                let Some(os) = map_u32(om, "slot") else { continue };
                if os == slot {
                    continue;
                }
                let oname = map_str(om, "name").unwrap_or("");
                if self.hide_builtins && Builtins::is_builtin_name(oname) {
                    continue;
                }
                let _ = write!(
                    out,
                    r#"<option value="{}"{}>"#,
                    os,
                    if cur_parent == Some(os) { " selected" } else { "" }
                );
                write_entity_label_escaped(out, om, os);
                let _ = out.try_push_str("</option>");
            }
        }
        let _ = out.try_push_str(r#"</select></div><span class="prop-sep"></span><div class="prop-group" title="Entity tags">"#);
        write_icon(out, Icon::Tag, 13, " prop-ico");

        if let Some(tags) = em.get("tags").and_then(Value::as_list) {
            for tv in tags {
                if let Some(t) = tv.as_str() {
                    let _ = out.try_push_str(r#"<span class="tag">"#);
                    self.write_highlighted(out, t, false);
                    let _ = out.try_push_str(r#"<button data-rm-tag=""#);
                    write_html_escaped(out, t);
                    let _ = out.try_push_str(r#"" type="button" title="Remove tag">"#);
                    write_icon(out, Icon::Close, 10, "");
                    let _ = out.try_push_str("</button></span>");
                }
            }
        }
        let _ = out.try_push_str(r#"<input class="tag-add" id="inTag" placeholder="+ tag" aria-label="Add tag"></div></div></div>"#);

        // Components toolbar
        let comps_map = em.get("components").and_then(Value::as_map);
        let comp_cnt = comps_map.map_or(0, Map::len);
        let all_shut = comp_cnt > 0
            && comps_map.is_some_and(|cm| {
                cm.iter().all(|(k, _)| self.collapsed_comps.iter().any(|c| c.as_str() == k))
            });

        let _ = out.try_push_str(r#"<div class="comp-bar"><div class="comp-bar-l"><span class="comp-bar-title">"#);
        write_icon(out, Icon::Layers, 13, "");
        let _ = write!(out, r#"<span>Components</span><span class="mono cnt-pill">{}</span></span>"#, comp_cnt);
        if comp_cnt > 0 {
            let _ = write!(
                out,
                r#"<button class="iconbtn sm" id="btnToggleAllComps" type="button" title="{}">"#,
                if all_shut { "Expand all components" } else { "Collapse all components" }
            );
            write_icon(out, if all_shut { Icon::Unfold } else { Icon::Fold }, 13, "");
            let _ = out.try_push_str("</button>");
        }
        let _ = out.try_push_str(r#"</div><div class="comp-bar-r">"#);

        // Attachable reflected components
        let mut attachable_any = false;
        if let Some(all_comps) = self.snapshot_components() {
            let mut opts = OwnString::new_in(self.alloc);
            for cv in all_comps {
                let Some(cm) = cv.as_map() else { continue };
                if !map_bool(cm, "reflected").unwrap_or(false) {
                    continue;
                }
                let Some(cname) = map_str(cm, "name") else { continue };
                if comps_map.is_some_and(|m| m.get(cname).is_some()) {
                    continue;
                }
                attachable_any = true;
                let _ = opts.try_push_str(r#"<option value=""#);
                write_html_escaped(&mut opts, cname);
                let _ = opts.try_push_str(r#"">"#);
                write_html_escaped(&mut opts, cname);
                let _ = opts.try_push_str("</option>");
            }
            if attachable_any {
                let _ = out.try_push_str(r#"<select class="flat-select framed" id="attachSel" aria-label="Attach component">"#);
                let _ = out.try_push_str(opts.as_str());
                let _ = out.try_push_str(r#"</select><button class="btn-subtle" id="btnAttach" type="button" title="Attach component">"#);
                write_icon(out, Icon::Plus, 12, "");
                let _ = out.try_push_str("<span>Attach</span></button>");
            }
        }
        let _ = out.try_push_str("</div></div>");

        // Dense 2-column component cards!
        if comp_cnt == 0 {
            let _ = out.try_push_str(r#"<div class="empty">No components attached.</div>"#);
            return;
        }
        let _ = out.try_push_str(r#"<div class="comp-grid">"#);
        if let Some(cm) = comps_map {
            for (cname, cval) in cm.iter() {
                let is_open = !self.collapsed_comps.iter().any(|c| c.as_str() == cname);
                let is_comp_cur = cur_match.is_some_and(|m| m.slot == slot && m.comp.as_str() == cname);
                let size_bytes = self.comp_size(cname);
                let c_ico = self.component_shape_icon(cname, cval);

                let _ = out.try_push_str(r#"<details class="comp-card" data-cname=""#);
                write_html_escaped(out, cname);
                let _ = write!(out, r#""{}><summary>"#, if is_open { " open" } else { "" });
                write_icon(out, Icon::Caret, 11, " chev");
                write_icon(out, c_ico, 13, " comp-ico");
                let _ = out.try_push_str(r#"<span class="comp-name">"#);
                self.write_highlighted(out, cname, is_comp_cur && cur_match.is_some_and(|m| m.kind == MatchKind::Comp));
                let _ = out.try_push_str("</span>");
                if let Some(sz) = size_bytes {
                    let _ = write!(out, r#"<span class="comp-size" title="{} bytes">{}B</span>"#, sz, sz);
                }
                let _ = out.try_push_str(r#"<span class="comp-status" id="st_"#);
                write_html_escaped(out, cname);
                let _ = out.try_push_str(r#""></span><button class="comp-copy" data-copy-comp=""#);
                write_html_escaped(out, cname);
                let _ = out.try_push_str(r#"" type="button" title="Copy JSON">"#);
                write_icon(out, Icon::Copy, 12, "");
                let _ = out.try_push_str(r#"</button></summary><div class="comp-body">"#);

                self.write_component_fields(out, cname, cval, cur_match, is_comp_cur);

                let _ = out.try_push_str("</div></details>");
            }
        }
        let _ = out.try_push_str("</div>");
    }

    fn write_component_fields(
        &self,
        out: &mut OwnString<A>,
        cname: &str,
        cval: &Value<A>,
        cur_match: Option<&FindMatch<A>>,
        is_comp_cur: bool,
    ) {
        if cval.as_str() == Some("<opaque>") {
            let _ = out.try_push_str(r#"<div class="well"><span class="well-k">value</span><span class="mono" style="margin-left:auto;font-size:11px;color:var(--ink-4)">opaque</span></div>"#);
            return;
        }

        // Gather ordered (name, type, value) field tuples from schema or live map
        let mut fields: OwnVec<(&str, &str, Option<&Value<A>>), A> = OwnVec::new_in(self.alloc);
        let schema = self.comp_schema(cname);
        if let Some(sm) = schema.and_then(Value::as_map) {
            if map_str(sm, "kind") == Some("struct") {
                if let Some(flist) = sm.get("fields").and_then(Value::as_list) {
                    let val_map = cval.as_map();
                    for fv in flist {
                        if let Some(fm) = fv.as_map() {
                            let fname = map_str(fm, "name").unwrap_or("");
                            let ftype = map_str(fm, "type").unwrap_or("str");
                            let v = val_map.and_then(|vm| vm.get(fname));
                            let _ = fields.try_push((fname, ftype, v));
                        }
                    }
                }
            }
        }

        if fields.is_empty() {
            if let Some(vm) = cval.as_map() {
                for (fk, fv) in vm.iter() {
                    let ftype = match fv {
                        Value::Bool(_) => "bool",
                        Value::Int(_) => "i64",
                        Value::Float(_) => "f64",
                        _ => "str",
                    };
                    let _ = fields.try_push((fk, ftype, Some(fv)));
                }
            } else {
                // Direct primitive/newtype value
                let ftype = schema
                    .and_then(Value::as_map)
                    .and_then(|m| map_str(m, "type"))
                    .unwrap_or("f64");
                let is_num = is_numeric_type(ftype);
                let _ = write!(out, r#"<div class="well" title="{}">"#, ftype);
                let _ = out.try_push_str(r#"<span class="well-k"#);
                if is_num {
                    let _ = out.try_push_str(r#" scrub" data-scrub="1""#);
                } else {
                    let _ = out.try_push_str(r#"""#);
                }
                let _ = out.try_push_str(r#">value</span><input type=""#);
                let _ = out.try_push_str(if is_num { "number" } else { "text" });
                let _ = out.try_push_str(r#"" step="any" data-comp=""#);
                write_html_escaped(out, cname);
                let _ = write!(out, r#"" data-direct="1" data-type="{}" value=""#, ftype);
                write_value_inline(out, cval);
                let _ = out.try_push_str(r#""></div>"#);
                return;
            }
        }

        // Pure structural field packing (zero hardcoded field-name tables!):
        // Any run of 2..=4 consecutive short numeric fields (name <= 4 chars, e.g. x/y/z, dx/dy/dz,
        // r/g/b/a, u/v, lo/hi, ax/ay/az) packs horizontally into an N-column strip; other fields
        // pack into a crisp 2-column property grid.
        let n = fields.len();
        let mut i = 0usize;
        let mut scalar_start = 0usize;
        let mut scalar_len = 0usize;

        while i < n {
            let mut run = 0usize;
            while run < 4 {
                let Some(f) = fields.get(i + run) else { break };
                if is_numeric_type(f.1) && f.0.len() <= 4 {
                    run += 1;
                } else {
                    break;
                }
            }
            if run >= 2 {
                if scalar_len > 0 {
                    if let Some(sl) = fields.get(scalar_start..scalar_start + scalar_len) {
                        self.write_scalar_grid(out, cname, sl, cur_match, is_comp_cur);
                    }
                    scalar_len = 0;
                }
                let _ = write!(out, r#"<div class="vec-row n{}">"#, run);
                if let Some(vsl) = fields.get(i..i + run) {
                    for f in vsl {
                        let is_fcur = is_comp_cur && cur_match.is_some_and(|m| m.field.as_str() == f.0);
                        self.write_field_well(out, cname, f.0, f.1, f.2, is_fcur);
                    }
                }
                let _ = out.try_push_str("</div>");
                i += run;
                scalar_start = i;
            } else {
                if scalar_len == 0 {
                    scalar_start = i;
                }
                scalar_len += 1;
                i += 1;
            }
        }
        if scalar_len > 0 {
            if let Some(sl) = fields.get(scalar_start..scalar_start + scalar_len) {
                self.write_scalar_grid(out, cname, sl, cur_match, is_comp_cur);
            }
        }
    }

    fn write_scalar_grid(
        &self,
        out: &mut OwnString<A>,
        cname: &str,
        slice: &[(&str, &str, Option<&Value<A>>)],
        cur_match: Option<&FindMatch<A>>,
        is_comp_cur: bool,
    ) {
        let _ = write!(out, r#"<div class="kv-grid{}">"#, if slice.len() == 1 { " n1" } else { "" });
        for &(fname, ftype, fval) in slice {
            let is_fcur = is_comp_cur && cur_match.is_some_and(|m| m.field.as_str() == fname);
            self.write_field_well(out, cname, fname, ftype, fval, is_fcur);
        }
        let _ = out.try_push_str("</div>");
    }

    fn write_field_well(
        &self,
        out: &mut OwnString<A>,
        cname: &str,
        fname: &str,
        ftype: &str,
        fval: Option<&Value<A>>,
        is_cur: bool,
    ) {
        let is_bool = ftype == "bool" || matches!(fval, Some(Value::Bool(_)));
        let is_num = !is_bool && is_numeric_type(ftype);

        if is_bool {
            let checked = matches!(fval, Some(Value::Bool(true)));
            let _ = out.try_push_str(r#"<label class="well well-bool" title=""#);
            write_html_escaped(out, fname);
            let _ = out.try_push_str(r#": bool"><span class="well-k">"#);
            self.write_highlighted(out, fname, is_cur);
            let _ = out.try_push_str(r#"</span><span class="sw-wrap"><input type="checkbox" data-comp=""#);
            write_html_escaped(out, cname);
            let _ = out.try_push_str(r#"" data-field=""#);
            write_html_escaped(out, fname);
            let _ = write!(
                out,
                r#"" data-type="bool"{}><span class="sw-track"></span><span class="sw-val">{}</span></span></label>"#,
                if checked { " checked" } else { "" },
                if checked { "true" } else { "false" }
            );
            return;
        }

        let wide = if fname.len() > 11 { " well-wide" } else { "" };
        let _ = write!(out, r#"<div class="well{}" title=""#, wide);
        write_html_escaped(out, fname);
        let _ = write!(
            out,
            r#": {}{}"><span class="well-k{}"{}>"#,
            ftype,
            if is_num { " — drag label to scrub" } else { "" },
            if is_num { " scrub" } else { "" },
            if is_num { r#" data-scrub="1""# } else { "" }
        );
        self.write_highlighted(out, fname, is_cur);
        let _ = out.try_push_str(r#"</span><input type=""#);
        let _ = out.try_push_str(if is_num { "number" } else { "text" });
        let _ = out.try_push_str(r#"" step="any" data-comp=""#);
        write_html_escaped(out, cname);
        let _ = out.try_push_str(r#"" data-field=""#);
        write_html_escaped(out, fname);
        let _ = write!(out, r#"" data-type="{}" value=""#, ftype);
        if let Some(v) = fval {
            write_value_inline(out, v);
        }
        let _ = out.try_push_str(r#""></div>"#);
    }

    /// Render the WebGPU Relational Graph / Live Spatial Viewport tab!
    fn write_graph_view(&self, out: &mut OwnString<A>) {
        let is_spatial = self.graph_mode == GraphMode::Spatial;
        let _ = out.try_push_str(r#"<div class="gpu-stage" id="gpuStage">"#);
        let _ = out.try_push_str(r#"<canvas class="gpu-canvas" id="gpuCanvas"></canvas>"#);
        let _ = out.try_push_str(r#"<canvas class="gpu-labels" id="gpuLabels"></canvas>"#);

        // Top-left compact WebGPU toolbar
        let _ = out.try_push_str(r#"<div class="gpu-hud-tl"><div class="seg">"#);
        let _ = write!(
            out,
            r#"<button class="seg-btn{}" data-gmode="relational" type="button" title="Force-directed relational ECS graph (ChildOf &amp; IsA)">"#,
            if !is_spatial { " active" } else { "" }
        );
        write_icon(out, Icon::Graph, 12, "");
        let _ = out.try_push_str("<span>Relational</span></button>");
        let _ = write!(
            out,
            r#"<button class="seg-btn{}" data-gmode="spatial" type="button" title="Live Transform (x,y,z) &amp; Velocity vectors">"#,
            if is_spatial { " active" } else { "" }
        );
        write_icon(out, Icon::Compass, 12, "");
        let _ = out.try_push_str("<span>Spatial</span></button></div>");
        let _ = out.try_push_str(r#"<button class="gpu-pill" id="btnGpuFit" type="button" title="Fit view (F)">"#);
        write_icon(out, Icon::Fit, 12, "");
        let _ = out.try_push_str(r#"</button><span class="gpu-badge" id="gpuBackendBadge">WebGPU · WASM ECS</span></div>"#);

        // Bottom-left selected node compact readout card (zero overlap, click to open Inspector)
        if let Some(slot) = self.selected_slot {
            if let Some(em) = self.entity_map_by_slot(slot) {
                let comp_cnt = em.get("components").and_then(Value::as_map).map_or(0, Map::len);
                let _ = out.try_push_str(r#"<div class="gpu-hud-bl"><div class="gpu-sel-row">"#);
                write_icon(out, if map_bool(em, "is_prefab").unwrap_or(false) { Icon::Diamond } else { Icon::Cube }, 13, " fire");
                let _ = out.try_push_str(r#"<span class="gpu-sel-name">"#);
                write_entity_label_escaped(out, em, slot);
                let _ = write!(out, r#"</span><span class="spill">"#);
                write_icon(out, Icon::Slot, 11, "");
                let _ = write!(out, r#"{}</span><span class="spill">"#, slot);
                write_icon(out, Icon::Layers, 11, "");
                let _ = write!(out, r#"{}</span>"#, comp_cnt);
                let _ = out.try_push_str(r#"<button class="btn-subtle" data-open-insp="1" type="button" title="Open in Inspector">"#);
                write_icon(out, Icon::Sliders, 11, "");
                let _ = out.try_push_str("<span>Inspect</span></button></div>");
                if let Some(entry) = self.entries.iter().find(|e| e.slot == slot) {
                    if let Ok(Some(sb)) = self.world.get::<SpatialBody>(entry.entity) {
                        if sb.has_transform {
                            let _ = write!(
                                out,
                                r#"<div class="gpu-sel-coords mono">x {:.1} · y {:.1} · z {:.1}{}</div>"#,
                                sb.x,
                                sb.y,
                                sb.z,
                                if sb.has_velocity { " · vel" } else { "" }
                            );
                        }
                    }
                }
                let _ = out.try_push_str("</div>");
            }
        }

        // Bottom-right quiet legend
        let _ = out.try_push_str(r#"<div class="gpu-hud-br"><span>drag node · scroll zoom · F fit</span></div>"#);
        let _ = out.try_push_str(r#"<div class="gpu-err" id="gpuErr" hidden></div></div>"#);
    }

    fn write_archetypes(&self, out: &mut OwnString<A>) {
        let archs = self.snapshot_archetypes();
        let cnt = archs.map_or(0, |a| a.len());
        let ent_cnt = self.snapshot_stat("entities");
        let _ = out.try_push_str(r#"<div class="view-head"><div class="vh-l">"#);
        write_icon(out, Icon::Table, 16, " title-ico");
        let _ = out.try_push_str("<h1>Archetypes</h1></div><div class=\"stat-pills\">");
        let _ = write!(out, r#"<span class="spill" title="{} tables">"#, cnt);
        write_icon(out, Icon::Table, 12, "");
        let _ = write!(out, r#"{}</span><span class="spill" title="{} entities">"#, cnt, ent_cnt);
        write_icon(out, Icon::Cube, 12, "");
        let _ = write!(out, r#"{}</span></div></div>"#, ent_cnt);

        let _ = out.try_push_str(r#"<table class="etable"><thead><tr><th style="width:58px">"#);
        write_icon(out, Icon::Table, 11, " th-ico");
        let _ = out.try_push_str(r#"</th><th style="width:68px">"#);
        write_icon(out, Icon::Cube, 11, " th-ico");
        let _ = out.try_push_str(r#"</th><th style="width:76px">Edges</th><th>Signature</th></tr></thead><tbody>"#);
        if let Some(list) = archs {
            for av in list {
                let Some(am) = av.as_map() else { continue };
                let id = map_u32(am, "id").unwrap_or(0);
                let ec = map_u32(am, "entity_count").unwrap_or(0);
                let add_e = map_u32(am, "add_edges").unwrap_or(0);
                let rm_e = map_u32(am, "remove_edges").unwrap_or(0);
                let _ = write!(
                    out,
                    r#"<tr><td class="mono muted">{}</td><td class="mono strong">{}</td><td class="mono faint">+{}/-{}</td><td><div class="sig-list">"#,
                    id, ec, add_e, rm_e
                );
                if let Some(comps) = am.get("components").and_then(Value::as_list) {
                    if comps.is_empty() {
                        let _ = out.try_push_str(r#"<span class="mono faint">[]</span>"#);
                    } else {
                        for cv in comps {
                            if let Some(cs) = cv.as_str() {
                                let is_rel = cs.starts_with('(');
                                let clean = cs.strip_prefix('#').unwrap_or(cs);
                                let _ = write!(out, r#"<span class="sig-chip{}">"#, if is_rel { " rel" } else { "" });
                                write_html_escaped(out, clean);
                                let _ = out.try_push_str("</span>");
                            }
                        }
                    }
                }
                let _ = out.try_push_str("</div></td></tr>");
            }
        }
        let _ = out.try_push_str("</tbody></table>");
    }

    fn write_query(&self, out: &mut OwnString<A>) {
        let _ = out.try_push_str(r#"<div class="view-head"><div class="vh-l">"#);
        write_icon(out, Icon::Funnel, 16, " title-ico");
        let _ = out.try_push_str(r#"<h1>Query</h1></div></div><div class="qbar"><input class="qinput" id="qWith" placeholder="With: Transform, Health" value=""#);
        write_html_escaped(out, self.query_with.as_str());
        let _ = out.try_push_str(r#""><input class="qinput" id="qWithout" placeholder="Without: Docked" value=""#);
        write_html_escaped(out, self.query_without.as_str());
        let _ = out.try_push_str(r#""><button class="btn-primary" id="btnRunQ" type="button">Run</button></div>"#);

        if let Some(rm) = self.query_result.as_ref().and_then(Value::as_map) {
            let cnt = map_u32(rm, "count").unwrap_or(0);
            let _ = write!(out, r#"<div class="stat-pills" style="margin-bottom:8px"><span class="spill">"#);
            write_icon(out, Icon::Cube, 12, "");
            let _ = write!(out, r#"{} matching</span></div>"#, cnt);
            let _ = out.try_push_str(r#"<table class="etable"><thead><tr><th>Entity</th><th>Tags</th><th>Components</th></tr></thead><tbody>"#);
            if let Some(ents) = rm.get("entities").and_then(Value::as_list) {
                for ev in ents {
                    let Some(em) = ev.as_map() else { continue };
                    let Some(slot) = map_u32(em, "slot") else { continue };
                    let _ = write!(out, r#"<tr style="cursor:pointer" data-qslot="{}"><td class="strong">"#, slot);
                    write_entity_label_escaped(out, em, slot);
                    let _ = write!(out, r#" <span class="mono faint">{}</span></td><td><div class="sig-list">"#, slot);
                    if let Some(tags) = em.get("tags").and_then(Value::as_list) {
                        for tv in tags {
                            if let Some(t) = tv.as_str() {
                                let _ = out.try_push_str(r#"<span class="sig-chip">"#);
                                write_html_escaped(out, t);
                                let _ = out.try_push_str("</span>");
                            }
                        }
                    }
                    let _ = out.try_push_str(r#"</div></td><td class="mono muted" style="font-size:11.5px">"#);
                    if let Some(cm) = em.get("components").and_then(Value::as_map) {
                        let mut first = true;
                        for (cn, _) in cm.iter() {
                            if !first { let _ = out.try_push_str(", "); }
                            first = false;
                            write_html_escaped(out, cn);
                        }
                    }
                    let _ = out.try_push_str("</td></tr>");
                }
            }
            let _ = out.try_push_str("</tbody></table>");
        } else {
            let _ = out.try_push_str(r#"<div class="empty">Filter entities by component or tag names (comma-separated).</div>"#);
        }
    }

    fn write_schemas(&self, out: &mut OwnString<A>) {
        let comps = self.snapshot_components();
        let cnt = comps.map_or(0, |c| c.len());
        let _ = out.try_push_str(r#"<div class="view-head"><div class="vh-l">"#);
        write_icon(out, Icon::Code, 16, " title-ico");
        let _ = write!(
            out,
            r#"<h1>Schemas</h1></div><span class="emeta">{} components · for&lt;F in fields(T)&gt;</span></div><div class="schema-grid">"#,
            cnt
        );
        if let Some(list) = comps {
            for cv in list {
                let Some(cm) = cv.as_map() else { continue };
                let cname = map_str(cm, "name").unwrap_or("Unknown");
                let cid = map_u32(cm, "id").unwrap_or(0);
                let sz = map_u32(cm, "size").unwrap_or(0);
                let _ = write!(
                    out,
                    r#"<pre class="schema-block"><span class="cm">// id {} · {}B · auto-Copy · inline for</span>\n"#,
                    cid, sz
                );
                let sm = cm.get("schema").and_then(Value::as_map);
                if sm.and_then(|m| map_str(m, "kind")) == Some("struct") {
                    let _ = out.try_push_str(r#"<span class="kw">struct</span> <span class="ty">"#);
                    write_html_escaped(out, cname);
                    let _ = out.try_push_str("</span> {\n");
                    if let Some(fl) = sm.and_then(|m| m.get("fields")).and_then(Value::as_list) {
                        for fv in fl {
                            if let Some(fm) = fv.as_map() {
                                let fn_s = map_str(fm, "name").unwrap_or("");
                                let ft_s = map_str(fm, "type").unwrap_or("");
                                let _ = out.try_push_str(r#"    <span class="kw">pub</span> "#);
                                write_html_escaped(out, fn_s);
                                let _ = out.try_push_str(r#": <span class="ty">"#);
                                write_html_escaped(out, ft_s);
                                let _ = out.try_push_str("</span>,\n");
                            }
                        }
                    }
                    let _ = out.try_push_str("}</pre>");
                } else {
                    let kind = sm.and_then(|m| map_str(m, "type")).unwrap_or("opaque");
                    let _ = out.try_push_str(r#"<span class="kw">struct</span> <span class="ty">"#);
                    write_html_escaped(out, cname);
                    let _ = out.try_push_str(r#"</span>(<span class="ty">"#);
                    write_html_escaped(out, kind);
                    let _ = out.try_push_str("</span>);</pre>");
                }
            }
        }
        let _ = out.try_push_str("</div>");
    }

    fn recompute_find(&mut self, keep_current: bool) {
        let prev = if keep_current {
            self.find_cur.and_then(|i| self.find_matches.get(i)).map(|m| (m.slot, m.kind))
        } else {
            None
        };
        let mut new_matches: OwnVec<FindMatch<A>, A> = OwnVec::new_in(self.alloc);
        let q = self.find_query.as_str().trim();
        if !q.is_empty() {
            if let Some(ents) = self.snapshot_entities() {
                for ev in ents {
                    let Some(em) = ev.as_map() else { continue };
                    let Some(slot) = map_u32(em, "slot") else { continue };
                    let name = map_str(em, "name").unwrap_or("");
                    if self.hide_builtins && Builtins::is_builtin_name(name) {
                        continue;
                    }
                    if contains_icase(name, q) {
                        let _ = new_matches.try_push(FindMatch {
                            slot,
                            kind: MatchKind::Entity,
                            comp: OwnString::new_in(self.alloc),
                            field: OwnString::new_in(self.alloc),
                        });
                    }
                    if let Some(tags) = em.get("tags").and_then(Value::as_list) {
                        for tv in tags {
                            if let Some(t) = tv.as_str() {
                                if contains_icase(t, q) {
                                    let _ = new_matches.try_push(FindMatch {
                                        slot,
                                        kind: MatchKind::Tag,
                                        comp: OwnString::new_in(self.alloc),
                                        field: OwnString::new_in(self.alloc),
                                    });
                                }
                            }
                        }
                    }
                    if let Some(cm) = em.get("components").and_then(Value::as_map) {
                        for (cn, cv) in cm.iter() {
                            if contains_icase(cn, q) {
                                if let Ok(cs) = OwnString::try_from_str_in(cn, self.alloc) {
                                    let _ = new_matches.try_push(FindMatch {
                                        slot,
                                        kind: MatchKind::Comp,
                                        comp: cs,
                                        field: OwnString::new_in(self.alloc),
                                    });
                                }
                            }
                            if let Some(vm) = cv.as_map() {
                                for (fk, fv) in vm.iter() {
                                    let mut val_str = OwnString::new_in(self.alloc);
                                    write_value_inline(&mut val_str, fv);
                                    if contains_icase(fk, q) || contains_icase(val_str.as_str(), q) {
                                        if let (Ok(cs), Ok(fs)) = (
                                            OwnString::try_from_str_in(cn, self.alloc),
                                            OwnString::try_from_str_in(fk, self.alloc),
                                        ) {
                                            let _ = new_matches.try_push(FindMatch {
                                                slot,
                                                kind: MatchKind::Field,
                                                comp: cs,
                                                field: fs,
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        self.find_matches = new_matches;

        if self.find_matches.is_empty() {
            self.find_cur = None;
        } else if let Some((ps, pk)) = prev {
            self.find_cur = self
                .find_matches
                .iter()
                .position(|m| m.slot == ps && m.kind == pk)
                .or(Some(0));
        } else {
            self.find_cur = self
                .selected_slot
                .and_then(|ss| self.find_matches.iter().position(|m| m.slot == ss))
                .or(Some(0));
        }
    }

    fn step_find(&mut self, delta: i64) {
        let n = self.find_matches.len();
        if n == 0 {
            return;
        }
        let cur = self.find_cur.unwrap_or(0);
        let next = if delta < 0 {
            if cur == 0 { n - 1 } else { cur - 1 }
        } else {
            (cur + 1) % n
        };
        self.find_cur = Some(next);
        self.reveal_current_match();
    }

    fn reveal_current_match(&mut self) {
        let Some(idx) = self.find_cur else { return };
        let Some(m) = self.find_matches.get(idx) else { return };
        let slot = m.slot;
        let comp_pos = if m.comp.as_str().is_empty() {
            None
        } else {
            self.collapsed_comps.iter().position(|c| c.as_str() == m.comp.as_str())
        };
        if let Some(p) = comp_pos {
            let _ = self.collapsed_comps.remove_swap(p);
        }
        self.expand_ancestors(slot);
        self.selected_slot = Some(slot);
        if self.active_view != ViewKind::Graph {
            self.active_view = ViewKind::Inspector;
        }
    }

    fn expand_ancestors(&mut self, slot: u32) {
        let mut cur = Some(slot);
        let mut guard = 0usize;
        while let Some(s) = cur {
            if guard >= 16 {
                break;
            }
            guard += 1;
            let parent = self
                .entity_map_by_slot(s)
                .and_then(|m| m.get("parents"))
                .and_then(Value::as_list)
                .and_then(|l| l.first())
                .and_then(val_u32);
            if let Some(p) = parent {
                if let Some(pos) = self.collapsed_tree.iter().position(|&c| c == p) {
                    let _ = self.collapsed_tree.remove_swap(pos);
                }
            }
            cur = parent;
        }
    }

    fn toggle_all_selected_comps(&mut self) {
        let Some(slot) = self.selected_slot else { return };
        let mut names: OwnVec<OwnString<A>, A> = OwnVec::new_in(self.alloc);
        if let Some(cm) = self
            .entity_map_by_slot(slot)
            .and_then(|m| m.get("components"))
            .and_then(Value::as_map)
        {
            for (cn, _) in cm.iter() {
                if let Ok(s) = OwnString::try_from_str_in(cn, self.alloc) {
                    let _ = names.try_push(s);
                }
            }
        }
        if names.is_empty() {
            return;
        }
        let all_shut = names
            .iter()
            .all(|n| self.collapsed_comps.iter().any(|c| c.as_str() == n.as_str()));
        if all_shut {
            for n in names.as_slice() {
                if let Some(p) = self.collapsed_comps.iter().position(|c| c.as_str() == n.as_str()) {
                    let _ = self.collapsed_comps.remove_swap(p);
                }
            }
        } else {
            for n in names.as_slice() {
                if !self.collapsed_comps.iter().any(|c| c.as_str() == n.as_str()) {
                    if let Ok(s) = OwnString::try_from_str_in(n.as_str(), self.alloc) {
                        let _ = self.collapsed_comps.try_push(s);
                    }
                }
            }
        }
    }

    fn ensure_selection(&mut self) {
        if let Some(s) = self.selected_slot {
            if self.entity_map_by_slot(s).is_some() {
                return;
            }
        }
        if let Some(ents) = self.snapshot_entities() {
            // Prefer first non-prefab multi-component entity (e.g. Flagship_Apollo)
            for ev in ents {
                let Some(em) = ev.as_map() else { continue };
                let Some(slot) = map_u32(em, "slot") else { continue };
                let name = map_str(em, "name").unwrap_or("");
                if self.hide_builtins && Builtins::is_builtin_name(name) {
                    continue;
                }
                if !map_bool(em, "is_prefab").unwrap_or(false)
                    && em.get("components").and_then(Value::as_map).is_some_and(|m| m.len() > 1)
                {
                    self.selected_slot = Some(slot);
                    return;
                }
            }
            if let Some(first) = self.entries.first() {
                self.selected_slot = Some(first.slot);
            }
        }
    }

    fn write_highlighted(&self, out: &mut OwnString<A>, text: &str, is_cur: bool) {
        let q = if self.find_open { self.find_query.as_str().trim() } else { "" };
        if q.is_empty() {
            write_html_escaped(out, text);
            return;
        }
        let tb = text.as_bytes();
        let qb = q.as_bytes();
        if qb.len() > tb.len() {
            write_html_escaped(out, text);
            return;
        }
        let mut pos = 0usize;
        while pos + qb.len() <= tb.len() {
            if matches_at_icase(tb, pos, qb) {
                if let Some(hit) = text.get(pos..pos + qb.len()) {
                    let _ = write!(out, r#"<mark class="fm{}">"#, if is_cur { " cur" } else { "" });
                    write_html_escaped(out, hit);
                    let _ = out.try_push_str("</mark>");
                    pos += qb.len();
                    continue;
                }
            }
            let ch_len = text.get(pos..).and_then(|s| s.chars().next()).map_or(1, char::len_utf8);
            if let Some(ch_s) = text.get(pos..pos + ch_len) {
                write_html_escaped(out, ch_s);
            }
            pos += ch_len;
        }
        if let Some(tail) = text.get(pos..) {
            if !tail.is_empty() {
                write_html_escaped(out, tail);
            }
        }
    }

    fn snapshot_entities(&self) -> Option<&[Value<A>]> {
        self.snapshot.as_ref()?.as_map()?.get("entities")?.as_list()
    }

    fn snapshot_prefabs(&self) -> Option<&[Value<A>]> {
        self.snapshot.as_ref()?.as_map()?.get("prefabs")?.as_list()
    }

    fn snapshot_components(&self) -> Option<&[Value<A>]> {
        self.snapshot.as_ref()?.as_map()?.get("components")?.as_list()
    }

    fn snapshot_archetypes(&self) -> Option<&[Value<A>]> {
        self.snapshot.as_ref()?.as_map()?.get("archetypes")?.as_list()
    }

    fn snapshot_stat(&self, key: &str) -> u32 {
        self.snapshot
            .as_ref()
            .and_then(Value::as_map)
            .and_then(|m| m.get("stats"))
            .and_then(Value::as_map)
            .and_then(|s| map_u32(s, key))
            .unwrap_or(0)
    }

    fn entity_map_by_slot(&self, slot: u32) -> Option<&Map<A>> {
        let ents = self.snapshot_entities()?;
        for ev in ents {
            if let Some(m) = ev.as_map() {
                if map_u32(m, "slot") == Some(slot) {
                    return Some(m);
                }
            }
        }
        None
    }

    fn comp_size(&self, cname: &str) -> Option<u32> {
        for cv in self.snapshot_components()? {
            if let Some(m) = cv.as_map() {
                if map_str(m, "name") == Some(cname) {
                    return map_u32(m, "size");
                }
            }
        }
        None
    }

    fn component_shape_icon(&self, cname: &str, cval: &Value<A>) -> Icon {
        if cval.as_str() == Some("<opaque>") {
            return schema_icon(0, 0, 0, true);
        }
        let mut total = 0usize;
        let mut num = 0usize;
        let mut bools = 0usize;
        if let Some(fl) = self
            .comp_schema(cname)
            .and_then(Value::as_map)
            .and_then(|m| m.get("fields"))
            .and_then(Value::as_list)
        {
            for fv in fl {
                if let Some(fm) = fv.as_map() {
                    total += 1;
                    let ft = map_str(fm, "type").unwrap_or("");
                    if ft == "bool" {
                        bools += 1;
                    } else if is_numeric_type(ft) {
                        num += 1;
                    }
                }
            }
        } else if let Some(vm) = cval.as_map() {
            for (_, fv) in vm.iter() {
                total += 1;
                match fv {
                    Value::Bool(_) => bools += 1,
                    Value::Int(_) | Value::Float(_) => num += 1,
                    _ => {}
                }
            }
        }
        schema_icon(total, num, bools, false)
    }

    fn comp_schema(&self, cname: &str) -> Option<&Value<A>> {
        for cv in self.snapshot_components()? {
            if let Some(m) = cv.as_map() {
                if map_str(m, "name") == Some(cname) {
                    return m.get("schema");
                }
            }
        }
        None
    }
}

fn extract_spatial<A: Allocator + Copy>(em: &Map<A>) -> SpatialBody {
    let mut sb = SpatialBody {
        x: 0.0,
        y: 0.0,
        z: 0.0,
        dx: 0.0,
        dy: 0.0,
        dz: 0.0,
        has_transform: false,
        has_velocity: false,
        active: true,
    };
    let Some(cm) = em.get("components").and_then(Value::as_map) else { return sb };
    if let Some(tm) = cm.get("Transform").and_then(Value::as_map) {
        sb.has_transform = true;
        sb.x = graph::f64_to_f32(map_f64(tm, "x").unwrap_or(0.0));
        sb.y = graph::f64_to_f32(map_f64(tm, "y").unwrap_or(0.0));
        sb.z = graph::f64_to_f32(map_f64(tm, "z").unwrap_or(0.0));
        sb.active = map_bool(tm, "active").unwrap_or(true);
    }
    if let Some(vm) = cm.get("Velocity").and_then(Value::as_map) {
        sb.has_velocity = true;
        sb.dx = graph::f64_to_f32(map_f64(vm, "dx").unwrap_or(0.0));
        sb.dy = graph::f64_to_f32(map_f64(vm, "dy").unwrap_or(0.0));
        sb.dz = graph::f64_to_f32(map_f64(vm, "dz").unwrap_or(0.0));
    }
    sb
}

fn is_numeric_type(t: &str) -> bool {
    matches!(
        t,
        "f32" | "f64" | "i8" | "i16" | "i32" | "i64" | "isize" | "u8" | "u16" | "u32" | "u64" | "usize" | "number"
    )
}

fn map_str<'a, A: Allocator + Copy>(m: &'a Map<A>, key: &str) -> Option<&'a str> {
    m.get(key)?.as_str()
}

fn map_bool<A: Allocator + Copy>(m: &Map<A>, key: &str) -> Option<bool> {
    match m.get(key)? {
        Value::Bool(b) => Some(*b),
        _ => None,
    }
}

fn map_i64<A: Allocator + Copy>(m: &Map<A>, key: &str) -> Option<i64> {
    match m.get(key)? {
        Value::Int(n) => n.as_i64(),
        _ => None,
    }
}

fn map_u32<A: Allocator + Copy>(m: &Map<A>, key: &str) -> Option<u32> {
    val_u32(m.get(key)?)
}

fn val_u32<A: Allocator + Copy>(v: &Value<A>) -> Option<u32> {
    match v {
        Value::Int(n) => n.as_u64().and_then(|u| u32::try_from(u).ok()),
        _ => None,
    }
}

fn map_f64<A: Allocator + Copy>(m: &Map<A>, key: &str) -> Option<f64> {
    match m.get(key)? {
        Value::Float(f) => Some(*f),
        Value::Int(n) => {
            let mut buf = blaze_own::FixedString::<64>::new();
            let _ = write!(buf, "{}", n);
            buf.as_str().parse::<f64>().ok().filter(|f| f.is_finite())
        }
        _ => None,
    }
}

fn write_entity_label_escaped<A: Allocator + Copy>(out: &mut OwnString<A>, em: &Map<A>, slot: u32) {
    let name = map_str(em, "name").unwrap_or("");
    if name.is_empty() {
        let _ = write!(out, "entity_{}", slot);
    } else {
        write_html_escaped(out, name);
    }
}

fn write_value_inline<A: Allocator + Copy>(out: &mut OwnString<A>, v: &Value<A>) {
    match v {
        Value::Null => {}
        Value::Bool(b) => { let _ = out.try_push_str(if *b { "true" } else { "false" }); }
        Value::Int(n) => { let _ = write!(out, "{}", n); }
        Value::Float(f) => { let _ = write!(out, "{}", f); }
        Value::Text(s) => write_html_escaped(out, s.as_str()),
        _ => {}
    }
}

fn write_html_escaped<W: core::fmt::Write>(out: &mut W, s: &str) {
    for ch in s.chars() {
        match ch {
            '&' => { let _ = out.write_str("&amp;"); }
            '<' => { let _ = out.write_str("&lt;"); }
            '>' => { let _ = out.write_str("&gt;"); }
            '"' => { let _ = out.write_str("&quot;"); }
            c => { let _ = out.write_char(c); }
        }
    }
}

fn matches_at_icase(hay: &[u8], pos: usize, needle: &[u8]) -> bool {
    let Some(sub) = hay.get(pos..pos + needle.len()) else { return false };
    sub.iter().zip(needle.iter()).all(|(a, b)| a.to_ascii_lowercase() == b.to_ascii_lowercase())
}

fn contains_icase(hay: &str, needle: &str) -> bool {
    let hb = hay.as_bytes();
    let nb = needle.as_bytes();
    if nb.is_empty() {
        return true;
    }
    if hb.len() < nb.len() {
        return false;
    }
    let mut i = 0usize;
    while i + nb.len() <= hb.len() {
        if matches_at_icase(hb, i, nb) {
            return true;
        }
        i += 1;
    }
    false
}
