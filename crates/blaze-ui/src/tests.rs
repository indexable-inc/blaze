//! Comprehensive unit tests for `blaze-ui`:
//! - Rust -> WGSL & MSL 2.4 shader generation and collision-free WebGPU bindings
//! - CPU software rasterizer validation of our SDF rounded-rect + border + clip math
//! - `IsA` Style Prefab live theme cascade & per-entity local override
//! - Deterministic two-pass flex/stack/scroll/clip layout
//! - Clip-aware hit-testing, ECS interaction tags, sliders, toggles, text input
//! - Binary-tree ECS docking (split, drag-to-dock, maximize, prune)
//! - End-to-end self-hosting `WorkbenchApp` frame rendering & live self-inspection

use core::fmt::Write as _;

use blaze_ecs::IncrementalCount;
use blaze_own::{String as OwnString, Vec as OwnVec};
use blaze_own::testing::heap;
use gpu::emit::StackWriter;
use gpu::prelude::{Position, Vec2 as GVec2, Vec4 as GVec4, vec2 as gvec2, vec4 as gvec4};

use crate::components::{
    BadgeTone, ComputedRect, Focused, Hovered, Inspects, LabelText, SliderState, TextInputState,
    ToggleState, TreeRowState, WidgetKind,
};
use crate::dock::{DropZone, SplitDir};
use crate::input::{InputEvent, Key, UiEvent};
use crate::shaders::{emit_ui_glyph_msl, emit_ui_glyph_wgsl, emit_ui_sdf_msl, emit_ui_sdf_wgsl};
use crate::style::{LayoutStyle, PaintStyle, Sizing};
use crate::types::{Edges, Rect, ThemeKind};
use crate::ui::UiContext;
use crate::virtual_list::VirtualList;
use crate::workbench::WorkbenchApp;

fn test_alloc() -> blaze_own::Alloc<'static> {
    heap().alloc()
}

#[test]
fn rust_authored_ui_shaders_emit_valid_wgsl_and_msl24() {
    let mut sdf_wgsl = StackWriter::<16384>::new();
    assert!(emit_ui_sdf_wgsl(&mut sdf_wgsl).is_ok());
    let sw = sdf_wgsl.as_str();
    assert!(sw.contains("@group(0) @binding(0) var<uniform> vp: Viewport;"));
    assert!(sw.contains("@group(0) @binding(1) var<storage, read> insts: array<RectInst>;"));
    assert!(sw.contains("@group(0) @binding(2) var<storage, read> clips: array<ClipRect>;"));
    assert!(!sw.contains("_gpu_sizes"));
    assert!(sw.contains("fn sd_round_box("));
    assert!(sw.contains("fn sd_segment("));
    assert!(sw.contains("@vertex"));
    assert!(sw.contains("@fragment"));
    assert!(!sw.contains("meta:"), "WGSL reserves `meta` as a keyword");
    assert!(sw.contains("@interpolate(flat) clip_id: u32,"));

    let mut glyph_wgsl = StackWriter::<16384>::new();
    assert!(emit_ui_glyph_wgsl(&mut glyph_wgsl).is_ok());
    let gw = glyph_wgsl.as_str();
    assert!(!gw.contains("meta:"), "WGSL reserves `meta` as a keyword");
    assert!(gw.contains("@group(0) @binding(0) var<uniform> vp: Viewport;"));
    assert!(gw.contains("@group(0) @binding(1) var<storage, read> glyphs: array<GlyphInst>;"));
    assert!(gw.contains("@group(0) @binding(2) var<storage, read> clips: array<ClipRect>;"));
    assert!(gw.contains("@group(0) @binding(3) var atlas_samp: sampler;"));
    assert!(gw.contains("@group(0) @binding(4) var atlas_tex: texture_2d<f32>;"));
    // Verify textureSample runs BEFORE discard for strict WebGPU uniformity compliance.
    let sample_pos = gw.find("textureSample(atlas_tex, atlas_samp, v.uv)").unwrap_or(usize::MAX);
    let discard_pos = gw.find("discard;").unwrap_or(0);
    assert!(sample_pos < discard_pos, "textureSample must execute before discard in WGSL");

    let mut sdf_msl = StackWriter::<16384>::new();
    assert!(emit_ui_sdf_msl(&mut sdf_msl).is_ok());
    assert!(sdf_msl.as_str().contains("#include <metal_stdlib>"));
    assert!(sdf_msl.as_str().contains("vertex ui_sdf_vs_out ui_sdf_vs("));
    assert!(sdf_msl.as_str().contains("fragment float4 ui_sdf_fs("));

    let mut glyph_msl = StackWriter::<16384>::new();
    assert!(emit_ui_glyph_msl(&mut glyph_msl).is_ok());
    assert!(glyph_msl.as_str().contains("atlas_tex.sample(atlas_samp, v.uv)"));
}

#[test]
fn cpu_software_rasterizer_verifies_sdf_rounded_rect_and_clip_math() {
    fn sd_round_box(p: GVec2, center: GVec2, half_ext: GVec2, r: f32) -> f32 {
        let rc = r.min(half_ext.x.min(half_ext.y));
        let q = gvec2((p.x - center.x).abs() - half_ext.x + rc, (p.y - center.y).abs() - half_ext.y + rc);
        let out = gvec2(q.x.max(0.0), q.y.max(0.0)).length();
        out + q.x.max(q.y).min(0.0) - rc
    }

    let mut fb = [GVec4::splat(0.0); 16 * 16];
    let quad: [GVec2; 6] = [
        gvec2(-1.0, -1.0),
        gvec2(1.0, -1.0),
        gvec2(1.0, 1.0),
        gvec2(-1.0, -1.0),
        gvec2(1.0, 1.0),
        gvec2(-1.0, 1.0),
    ];
    gpu::cpu::rasterize_triangles(
        16,
        16,
        2,
        1,
        &mut fb,
        None,
        |vid, _iid| {
            let k = quad[usize::try_from(vid.get()).unwrap_or(0)];
            (Position(gvec4(k.x, k.y, 0.0, 1.0)), gvec2((k.x + 1.0) * 8.0, (1.0 - k.y) * 8.0))
        },
        |a, b, c, bary| *a * bary.x + *b * bary.y + *c * bary.z,
        |_frag_pos, ctx| {
            let p = *ctx.value();
            // Rounded rect [2,2..14,14] r=3 clipped to left half [0,0..8,16]
            let cd = sd_round_box(p, gvec2(4.0, 8.0), gvec2(4.0, 8.0), 0.0);
            let clip_a = (0.5 - cd).clamp(0.0, 1.0);
            if clip_a <= 0.01 {
                return gpu::cpu::FragOutcome::Discard;
            }
            let d = sd_round_box(p, gvec2(8.0, 8.0), gvec2(6.0, 6.0), 3.0);
            let a = (0.5 - d).clamp(0.0, 1.0) * clip_a;
            if a <= 0.01 {
                return gpu::cpu::FragOutcome::Discard;
            }
            gpu::cpu::FragOutcome::Keep(gvec4(0.88, 0.28, 0.10, a))
        },
    );

    // Interior pixel inside clip (x=5, y=8) is shaded; right-side pixel (x=11, y=8) is clipped!
    assert!(fb[8 * 16 + 5].w > 0.9);
    assert!(fb[8 * 16 + 11].w < 0.01);
}

#[test]
fn isa_style_prefab_cascades_live_and_allows_local_override() {
    let alloc = test_alloc();
    let mut ui = UiContext::new_in(alloc).expect("ui init");
    let b1 = ui.spawn_button(ui.root, "BtnA", "Save", 1, false).expect("b1");
    let b2 = ui.spawn_button(ui.root, "BtnB", "Cancel", 2, false).expect("b2");

    // Both buttons inherit Obsidian button style via `IsA(Style::Button)`.
    let s1_dark = ui.world.get_inherited::<PaintStyle>(b1).expect("get").copied().expect("style");
    let s2_dark = ui.world.get_inherited::<PaintStyle>(b2).expect("get").copied().expect("style");
    assert_eq!(s1_dark.bg, s2_dark.bg);

    // Give `b2` a local radius override; switch theme to Paper -> `b1` updates live, `b2` keeps local.
    let mut custom = s2_dark;
    custom.radius = 15.0;
    ui.world.set_copy(b2, custom).expect("override");

    ui.set_theme(ThemeKind::Paper).expect("switch theme");
    let s1_light = ui.world.get_inherited::<PaintStyle>(b1).expect("get").copied().expect("style");
    let s2_light = ui.world.get_inherited::<PaintStyle>(b2).expect("get").copied().expect("style");
    assert_ne!(s1_light.bg, s1_dark.bg, "IsA prefab cascade must update b1 live");
    assert_eq!(s2_light.radius, 15.0, "local component override must win over prefab");
}

#[test]
fn two_pass_layout_flex_clip_and_input_interaction_work_end_to_end() {
    let alloc = test_alloc();
    let mut ui = UiContext::new_in(alloc).expect("ui init");
    let bar = ui.spawn_toolbar(ui.root, "Bar", 40.0).expect("bar");
    let btn = ui.spawn_button(bar, "RunBtn", "Run", 77, true).expect("btn");
    let body = ui
        .spawn_container(
            ui.root,
            "Body",
            crate::WidgetKind::Panel,
            LayoutStyle::column().fill().pad(Edges::all(10.0)).gap(8.0).clip(true),
            Some(ui.prefabs.panel),
        )
        .expect("body");
    let tog = ui.spawn_toggle(body, "FlagToggle", "Enable LTO", false, 88).expect("tog");
    let sld = ui
        .spawn_slider(body, "ScaleSlider", "Scale", SliderState::new(0.5, 0.0, 1.0, 0.1, 1), 89)
        .expect("sld");
    let inp = ui.spawn_text_input(body, "QueryInput", "", "Search...", 90).expect("inp");

    ui.render_frame(800.0, 600.0, 2.0, 0.0, None).expect("frame");

    let bar_r = ui.world.get::<ComputedRect>(bar).expect("g").copied().expect("cr");
    let body_r = ui.world.get::<ComputedRect>(body).expect("g").copied().expect("cr");
    assert_eq!(bar_r.rect.h, 40.0);
    assert_eq!(body_r.rect.h, 560.0);
    assert!(body_r.child_clip_id > 0, "clipped panel registers a GPU ClipRect");

    // Hover & click button -> updates `Hovered` tag and emits `UiEvent::Click { action: 77 }`.
    let btn_r = ui.world.get::<ComputedRect>(btn).expect("g").copied().expect("cr").rect;
    let bc = btn_r.center();
    ui.handle_input(InputEvent::PointerMove { x: bc.x, y: bc.y }).expect("move");
    assert!(ui.world.has_tag_typed::<Hovered>(btn).expect("tag"));
    ui.handle_input(InputEvent::PointerDown { x: bc.x, y: bc.y, button: 0, shift: false }).expect("down");
    ui.handle_input(InputEvent::PointerUp { x: bc.x, y: bc.y, button: 0 }).expect("up");

    // Click toggle -> flips `ToggleState::checked` to true.
    let tc = ui.world.get::<ComputedRect>(tog).expect("g").copied().expect("cr").rect.center();
    ui.handle_input(InputEvent::PointerDown { x: tc.x, y: tc.y, button: 0, shift: false }).expect("down");
    ui.handle_input(InputEvent::PointerUp { x: tc.x, y: tc.y, button: 0 }).expect("up");
    assert!(ui.world.get::<ToggleState>(tog).expect("g").copied().expect("t").checked);

    // Drag slider to far right -> updates `SliderState::value` to 1.0.
    let sr = ui.world.get::<ComputedRect>(sld).expect("g").copied().expect("cr").rect;
    ui.handle_input(InputEvent::PointerDown { x: sr.right() - 2.0, y: sr.center().y, button: 0, shift: false })
        .expect("down");
    ui.handle_input(InputEvent::PointerUp { x: sr.right() - 2.0, y: sr.center().y, button: 0 }).expect("up");
    assert!((ui.world.get::<SliderState>(sld).expect("g").copied().expect("s").value - 1.0).abs() < 1e-4);

    // Focus TextInput & type "ecs" then Backspace -> "ec".
    let ic = ui.world.get::<ComputedRect>(inp).expect("g").copied().expect("cr").rect.center();
    ui.handle_input(InputEvent::PointerDown { x: ic.x, y: ic.y, button: 0, shift: false }).expect("down");
    ui.handle_input(InputEvent::PointerUp { x: ic.x, y: ic.y, button: 0 }).expect("up");
    assert!(ui.world.has_tag_typed::<Focused>(inp).expect("foc"));
    for ch in ['e', 'c', 's'] {
        ui.handle_input(InputEvent::TextChar(ch)).expect("char");
    }
    ui.handle_input(InputEvent::KeyDown { key: Key::Backspace, shift: false, ctrl_or_meta: false }).expect("bs");
    assert_eq!(
        ui.world
            .get::<TextInputState<blaze_own::Alloc<'static>>>(inp)
            .expect("g")
            .expect("st")
            .text
            .as_str(),
        "ec"
    );

    let mut evs = OwnVec::new_in(alloc);
    ui.drain_events(&mut evs).expect("drain");
    assert!(evs.iter().any(|e| matches!(e, UiEvent::Click { action: 77, .. })));
    assert!(evs.iter().any(|e| matches!(e, UiEvent::ToggleChanged { action: 88, checked: true, .. })));
    assert!(evs.iter().any(|e| matches!(e, UiEvent::SliderChanged { action: 89, .. })));
    assert!(evs.iter().any(|e| matches!(e, UiEvent::TextChanged { action: 90, .. })));
}

#[test]
fn ecs_dock_workspace_splits_drags_maximizes_and_prunes_empty_leaves() {
    let alloc = test_alloc();
    let mut ui = UiContext::new_in(alloc).expect("ui");
    let p1 = ui.spawn_pane_root("P1", false).expect("p1");
    let p2 = ui.spawn_pane_root("P2", false).expect("p2");
    let p3 = ui.spawn_pane_root("P3", false).expect("p3");

    let l1 = ui.dock.spawn_leaf(&mut ui.world, "L1").expect("l1");
    let l2 = ui.dock.spawn_leaf(&mut ui.world, "L2").expect("l2");
    let t1 = ui.dock.add_tab(&mut ui.world, l1, "Tab1", 1, p1).expect("t1");
    let t2 = ui.dock.add_tab(&mut ui.world, l2, "Tab2", 2, p2).expect("t2");
    let _t3 = ui.dock.add_tab(&mut ui.world, l2, "Tab3", 3, p3).expect("t3");
    let split = ui.dock.spawn_split(&mut ui.world, "RootSplit", SplitDir::Horizontal, 0.5, l1, l2).expect("sp");
    ui.dock.root = Some(split);

    let bounds = Rect::new(0.0, 0.0, 1000.0, 600.0);
    ui.dock.compute_layout(&ui.world, bounds).expect("layout");
    assert_eq!(ui.dock.leaves.len(), 2);
    assert_eq!(ui.dock.splitters.len(), 1);

    // Drag Tab1 onto Leaf2's Center -> Leaf1 becomes empty and auto-prunes so Leaf2 is new root!
    assert!(ui.dock.drop_tab(&mut ui.world, t1, l1, l2, DropZone::Center).expect("drop"));
    ui.dock.compute_layout(&ui.world, bounds).expect("layout2");
    assert_eq!(ui.dock.leaves.len(), 1);
    assert_eq!(ui.dock.tabs_of(&ui.world, l2).expect("tabs").len(), 3);

    // Drag Tab2 onto Leaf2's Bottom -> creates a vertical split with 2 leaves again.
    assert!(ui.dock.drop_tab(&mut ui.world, t2, l2, l2, DropZone::Bottom).expect("drop2"));
    ui.dock.compute_layout(&ui.world, bounds).expect("layout3");
    assert_eq!(ui.dock.leaves.len(), 2);

    // Maximize Tab2 -> single full-viewport leaf; toggle again -> restores 2 leaves.
    ui.dock.toggle_maximize(t2);
    ui.dock.compute_layout(&ui.world, bounds).expect("max");
    assert_eq!(ui.dock.leaves.len(), 1);
    assert_eq!(ui.dock.leaves.first().expect("l").rect, bounds);
    ui.dock.toggle_maximize(t2);
    ui.dock.compute_layout(&ui.world, bounds).expect("restore");
    assert_eq!(ui.dock.leaves.len(), 2);
}

#[test]
fn self_hosting_workbench_renders_full_gpu_buffers_and_inspects_own_ecs() {
    let alloc = test_alloc();
    let mut app = WorkbenchApp::new_in(alloc).expect("workbench");
    app.render_frame(1440.0, 900.0, 2.0, 0.5).expect("frame 1");
    app.render_frame(1440.0, 900.0, 2.0, 1.0).expect("frame 2");

    assert!(app.ui.draw.clips.len() >= 4, "nested rounded GPU clips registered");
    assert!(app.ui.draw.rects.len() >= 40, "rich SDF UI geometry emitted");
    assert!(app.ui.draw.glyphs.len() >= 200, "instanced Berkeley Mono glyphs emitted");

    // Verify self-inspection (`Inspects` relation points to a live entity in the UI world).
    let inspects_rel = app.ui.world.relation_entity::<Inspects>().expect("rel");
    let targets = app.ui.world.targets_of(app.ui.world.entity_by_name("Pane::Inspector").expect("pane"), inspects_rel).expect("t");
    assert_eq!(targets.as_slice(), &[app.inspected]);

    // Switch theme live via `IsA` prefab cascade and re-render cleanly.
    app.ui.set_theme(ThemeKind::Paper).expect("paper");
    app.render_frame(1440.0, 900.0, 2.0, 1.5).expect("frame 3");
    assert_eq!(app.ui.theme.kind, ThemeKind::Paper);
}

#[test]
fn incremental_3000_rows_sparse_edit_noop_hover_reactive_and_boundary() {
    let alloc = test_alloc();
    let mut ui = UiContext::new_in(alloc).expect("ui");

    // Top bar with a fixed-width reactive `unread` badge + a fixed-size sibling card (layout boundary!).
    let bar = ui.spawn_toolbar(ui.root, "BenchBar", 36.0).expect("bar");
    let unread_badge = ui.spawn_badge(bar, "UnreadBadge", "0 unread", BadgeTone::Fire).expect("badge");
    ui.world
        .set_copy(unread_badge, LayoutStyle::row().width(Sizing::Fixed(110.0)).height(Sizing::Fixed(20.0)))
        .expect("fixed badge");

    let card = ui
        .spawn_container(
            ui.root,
            "SiblingCard",
            WidgetKind::Card,
            LayoutStyle::row().width(Sizing::Fixed(320.0)).height(Sizing::Fixed(32.0)).pad(Edges::all(4.0)),
            Some(ui.prefabs.card),
        )
        .expect("card");
    let auto_lbl = ui.spawn_label(card, "AutoCardLabel", "Ready", false).expect("auto lbl");

    // Scrollable 3,000-row list mirroring zortax's "A Critique of GPUI" sparse-update benchmark.
    let list = ui
        .spawn_container(
            ui.root,
            "List3000",
            WidgetKind::Panel,
            LayoutStyle::column().fill().scrollable(true).clip(true),
            Some(ui.prefabs.panel),
        )
        .expect("list");

    let mut unread = IncrementalCount::<u32, _>::new_in(&mut ui.reactive, alloc).expect("inc");
    let mut rows = OwnVec::try_with_capacity_in(3000, alloc).expect("cap");
    let mut name_buf = OwnString::new_in(alloc);
    let mut lbl_buf = OwnString::new_in(alloc);
    for i in 0u32..3000u32 {
        name_buf.clear();
        lbl_buf.clear();
        let _ = write!(name_buf, "Row{i}");
        let _ = write!(lbl_buf, "Thread #{i:04}");
        let e = ui
            .spawn_tree_row(
                list,
                name_buf.as_str(),
                lbl_buf.as_str(),
                "meta",
                TreeRowState { depth: 0, has_children: false, expanded: false, target: None },
                10_000 + i,
            )
            .expect("row");
        rows.try_push(e).expect("push");
        let _ = unread.upsert(&mut ui.reactive, i, i % 10 == 0).expect("upsert");
    }

    // Bind unread badge as a run-once reactive hole (closure runs ONLY when unread count changes!).
    let unread_sig = unread.signal();
    ui.bind_label(unread_badge, move |cx, out| {
        let n = cx.val(unread_sig);
        let _ = write!(out, "{n} unread");
    })
    .expect("bind");

    // Frame 1: initial full layout + retained GPU slab extraction.
    ui.render_frame(1200.0, 800.0, 2.0, 0.0, None).expect("frame 1");
    assert!(ui.draw.full_upload);
    assert!(ui.last_stats.layout_nodes_visited >= 3000);
    assert!(ui.last_stats.paint_nodes_visited >= 3000);
    assert_eq!(
        ui.world.get::<LabelText<blaze_own::Alloc<'static>>>(unread_badge).expect("g").expect("lbl").0.as_str(),
        "300 unread"
    );

    // Frame 2: Idle frame -> TRUE 0 µs / 0 layout / 0 paint / 0 GPU bytes!
    ui.render_frame(1200.0, 800.0, 2.0, 0.016, None).expect("frame 2 idle");
    assert!(!ui.draw.full_upload);
    assert_eq!(ui.last_stats.layout_nodes_visited, 0);
    assert_eq!(ui.last_stats.paint_nodes_visited, 0);
    assert_eq!(ui.last_stats.gpu_bytes_patched, 0);

    // Frame 3: Equal write (no-op) on row 1500 -> equality-guarded, 0 work!
    let row_1500 = rows[1500];
    assert!(!ui.set_label_text(row_1500, "Thread #1500").expect("noop"));
    ui.render_frame(1200.0, 800.0, 2.0, 0.032, None).expect("frame 3 noop");
    assert!(!ui.draw.full_upload);
    assert_eq!(ui.last_stats.layout_nodes_visited, 0);
    assert_eq!(ui.last_stats.paint_nodes_visited, 0);
    assert_eq!(ui.last_stats.gpu_bytes_patched, 0);

    // Frame 4: Sparse 1-label edit at 3,000 rows -> 0 layout nodes, 1 painted entity, partial GPU patch!
    assert!(ui.set_label_text(row_1500, "Thread #1500 patched!").expect("edit"));
    ui.render_frame(1200.0, 800.0, 2.0, 0.048, None).expect("frame 4 sparse");
    assert!(!ui.draw.full_upload);
    assert_eq!(ui.last_stats.layout_nodes_visited, 0);
    assert_eq!(ui.last_stats.paint_nodes_visited, 1);
    assert!(ui.last_stats.gpu_bytes_patched > 0 && ui.last_stats.gpu_bytes_patched < 4096);

    // Frame 5: Hover row 2 at 3,000 rows -> Flecs 4.2 DontFragment (0 table migrations!), 0 layout, <= 2 painted!
    let tables_before = ui.world.table_count();
    let r2_rect = ui.world.get::<ComputedRect>(rows[2]).expect("g").copied().expect("cr").rect;
    ui.handle_input(InputEvent::PointerMove { x: r2_rect.center().x, y: r2_rect.center().y })
        .expect("hover");
    assert!(ui.world.has_tag_typed::<Hovered>(rows[2]).expect("hovered"));
    assert_eq!(
        ui.world.table_count(),
        tables_before,
        "DontFragment Hovered tag must cause ZERO archetype table migrations"
    );
    ui.render_frame(1200.0, 800.0, 2.0, 0.064, None).expect("frame 5 hover");
    assert!(!ui.draw.full_upload);
    assert_eq!(ui.last_stats.layout_nodes_visited, 0);
    assert!(ui.last_stats.paint_nodes_visited >= 1 && ui.last_stats.paint_nodes_visited <= 2);
    assert!(ui.last_stats.gpu_bytes_patched > 0 && ui.last_stats.gpu_bytes_patched < 4096);

    // Frame 6: Toggle 1 unread flag out of 3,000 -> O(1) IncrementalCount delta + 1 reactive hole eval + 0 layout!
    assert!(unread.upsert(&mut ui.reactive, 1500, false).expect("toggle"));
    ui.render_frame(1200.0, 800.0, 2.0, 0.080, None).expect("frame 6 reactive");
    assert!(!ui.draw.full_upload);
    assert_eq!(ui.last_stats.layout_nodes_visited, 0);
    assert_eq!(ui.last_stats.paint_nodes_visited, 1);
    assert!(ui.last_stats.reactive_evals >= 1);
    assert_eq!(
        ui.world.get::<LabelText<blaze_own::Alloc<'static>>>(unread_badge).expect("g").expect("lbl").0.as_str(),
        "299 unread"
    );

    // Frame 7: Mutating an Auto-sized label inside a fixed-size sibling card stops at the layout boundary!
    assert!(ui.set_label_text(auto_lbl, "Localized Layout Boundary!").expect("auto"));
    ui.render_frame(1200.0, 800.0, 2.0, 0.096, None).expect("frame 7 boundary");
    assert!(!ui.draw.full_upload);
    assert!(
        ui.last_stats.layout_nodes_visited > 0 && ui.last_stats.layout_nodes_visited <= 4,
        "layout must stop at SiblingCard boundary instead of re-laying out 3,000 rows (got {})",
        ui.last_stats.layout_nodes_visited
    );
}

#[test]
fn virtual_list_100k_rows_fixed_and_variable_heights() {
    let alloc = test_alloc();
    let mut ui = UiContext::new_in(alloc).expect("ui");
    let container = ui
        .spawn_container(
            ui.root,
            "VListHost",
            WidgetKind::Panel,
            LayoutStyle::column().fill().clip(true),
            Some(ui.prefabs.panel),
        )
        .expect("host");

    // 1. Fixed-height VirtualList over 100,000 items backed by a fixed 30-entity pool.
    let mut vlist = VirtualList::new_fixed_in(100_000, 24.0, alloc);
    vlist.mount_pool(&mut ui, container, "VRow", 30, 50_000).expect("mount");
    assert_eq!(vlist.total_items(), 100_000);
    assert_eq!(vlist.pool.len(), 30);
    let entities_after_mount = ui.world.live_count();

    let bind = |idx: usize, lbl: &mut OwnString<_>, sub: &mut OwnString<_>| {
        let _ = write!(lbl, "Item #{idx:06}");
        let _ = write!(sub, "row {idx}");
    };
    let _ = vlist.sync_visible(&mut ui, 600.0, bind).expect("sync 0");
    ui.render_frame(800.0, 600.0, 2.0, 0.0, None).expect("initial");
    assert_eq!(ui.world.live_count(), entities_after_mount);

    // Jump straight to row 50,000 -> zero new ECS entities, zero layout nodes!
    vlist.scroll_by(50_000.0 * 24.0, 600.0);
    let updated_mid = vlist.sync_visible(&mut ui, 600.0, bind).expect("sync 50k");
    assert!(updated_mid > 0);
    let vis_mid = vlist.visible_range(600.0, 0);
    assert_eq!(vis_mid.first_row, 50_000);
    assert_eq!(ui.world.live_count(), entities_after_mount);
    assert_eq!(
        ui.world.get::<LabelText<blaze_own::Alloc<'static>>>(vlist.pool[0]).expect("g").expect("l").0.as_str(),
        "Item #050000"
    );
    ui.render_frame(800.0, 600.0, 2.0, 0.016, None).expect("frame 50k");
    assert_eq!(ui.last_stats.layout_nodes_visited, 0);
    assert!(!ui.draw.full_upload);

    // Jump near the tail (row 99,990) -> clamps cleanly and hides unused pool slots.
    vlist.scroll_by(50_000.0 * 24.0, 600.0);
    let _ = vlist.sync_visible(&mut ui, 600.0, bind).expect("sync end");
    let vis_end = vlist.visible_range(600.0, 0);
    assert!(vis_end.first_row + vis_end.count <= 100_000);
    assert_eq!(ui.world.live_count(), entities_after_mount);

    // 2. Variable-height Fenwick VirtualList over 10,000 rows (heights 20..59px).
    let mut heights = OwnVec::try_with_capacity_in(10_000, alloc).expect("cap");
    let mut expected_total = 0.0f32;
    for i in 0u32..10_000u32 {
        let h = 20.0 + f32::from(u16::try_from(i % 40).unwrap_or(0));
        heights.try_push(h).expect("push");
        expected_total += h;
    }
    let mut var_list = VirtualList::new_variable_in(heights.as_slice(), alloc).expect("var");
    assert!((var_list.total_height() - expected_total).abs() < 1.0);
    let off_500 = var_list.offset_of(500);
    assert_eq!(var_list.row_at_offset(off_500 + 1.0), 500);
    var_list.set_row_height(100, 120.0);
    let delta = 120.0 - heights[100];
    assert!((var_list.offset_of(500) - (off_500 + delta)).abs() < 0.01);
}

#[test]
fn workbench_100k_lab_actions_patch_incrementally() {
    let alloc = test_alloc();
    let mut app = WorkbenchApp::new_in(alloc).expect("workbench");
    app.render_frame(1440.0, 900.0, 2.0, 0.0).expect("frame 0");

    // Select the "100K Lab" tab (index 2 in Leaf::Viewport: GPU Canvas, Shader IR, 100K Lab).
    let leaf_mid = app.ui.world.entity_by_name("Leaf::Viewport").expect("leaf");
    app.ui.dock.select_tab(&mut app.ui.world, leaf_mid, 2).expect("select");
    app.ui.mark_full_dirty();
    app.render_frame(1440.0, 900.0, 2.0, 0.1).expect("frame lab");
    // Flush telemetry badge sync once, then verify true 0-work idle frame!
    app.render_frame(1440.0, 900.0, 2.0, 0.2).expect("telemetry flush");
    app.render_frame(1440.0, 900.0, 2.0, 0.3).expect("idle");
    assert_eq!(app.ui.last_stats.layout_nodes_visited, 0);
    assert_eq!(app.ui.last_stats.paint_nodes_visited, 0);
    assert_eq!(app.ui.last_stats.gpu_bytes_patched, 0);

    // Click "Edit 1 Label" button -> triggers 0 layout nodes and partial GPU patch!
    let edit_btn = app.ui.world.entity_by_name("LabEditOne").expect("edit btn");
    let ec = app.ui.world.get::<ComputedRect>(edit_btn).expect("g").copied().expect("cr").rect.center();
    app.handle_input(InputEvent::PointerDown { x: ec.x, y: ec.y, button: 0, shift: false }).expect("down");
    app.handle_input(InputEvent::PointerUp { x: ec.x, y: ec.y, button: 0 }).expect("up");
    app.render_frame(1440.0, 900.0, 2.0, 0.4).expect("after edit");
    assert!(!app.ui.draw.full_upload, "Edit 1 Label must use partial GPU buffer patching");
    assert_eq!(app.ui.last_stats.layout_nodes_visited, 0);
    assert!(app.ui.last_stats.gpu_bytes_patched > 0 && app.ui.last_stats.gpu_bytes_patched < 8192);

    // Click "Toggle Unread" -> updates reactive unread badge with 0 layout nodes!
    let tog_btn = app.ui.world.entity_by_name("LabToggleUnread").expect("tog btn");
    let tc = app.ui.world.get::<ComputedRect>(tog_btn).expect("g").copied().expect("cr").rect.center();
    app.handle_input(InputEvent::PointerDown { x: tc.x, y: tc.y, button: 0, shift: false }).expect("down");
    app.handle_input(InputEvent::PointerUp { x: tc.x, y: tc.y, button: 0 }).expect("up");
    app.render_frame(1440.0, 900.0, 2.0, 0.5).expect("after toggle");
    assert!(!app.ui.draw.full_upload);
    assert_eq!(app.ui.last_stats.layout_nodes_visited, 0);
    assert!(app.ui.last_stats.reactive_evals >= 1);

    // Click "Scroll +500" -> recycles 24 pooled ECS entities to rows 500..524 with 0 layout nodes!
    let scroll_btn = app.ui.world.entity_by_name("LabScroll100k").expect("scroll btn");
    let sc = app.ui.world.get::<ComputedRect>(scroll_btn).expect("g").copied().expect("cr").rect.center();
    app.handle_input(InputEvent::PointerDown { x: sc.x, y: sc.y, button: 0, shift: false }).expect("down");
    app.handle_input(InputEvent::PointerUp { x: sc.x, y: sc.y, button: 0 }).expect("up");
    app.render_frame(1440.0, 900.0, 2.0, 0.6).expect("after scroll");
    assert!(!app.ui.draw.full_upload);
    assert_eq!(app.ui.last_stats.layout_nodes_visited, 0);
}

#[test]
fn rust_gpu_slug_shader_emits_wgsl_and_msl_and_evaluates_on_cpu_at_any_zoom() {
    use crate::slug::{Outline, PathBuilder, pack_outline_into, slug_render_cpu};
    use crate::slug_shader::{
        SiteUniform, SlugSiteVaryings, emit_slug_site_msl, emit_slug_site_wgsl, slug_site_fs,
    };

    // 1. Verify real Rust `#[gpu::vertex(ir)]` + `#[gpu::fragment(ir)]` + `#[gpu::helper]`
    //    emits linked WebGPU WGSL and Metal MSL 2.4 cleanly!
    let mut wgsl = StackWriter::<32768>::new();
    assert!(emit_slug_site_wgsl(&mut wgsl).is_ok());
    let w = wgsl.as_str();
    assert!(w.contains("fn slug_root_code("));
    assert!(w.contains("fn slug_solve_horiz("));
    assert!(w.contains("fn slug_solve_vert("));
    assert!(w.contains("@group(0) @binding(0) var<uniform> u: SiteUniform;"));
    assert!(w.contains("@group(0) @binding(1) var<storage, read> insts: array<SiteInst>;"));
    assert!(w.contains("@group(0) @binding(2) var<storage, read> curves: array<vec4<f32>>;"));
    assert!(w.contains("@group(0) @binding(3) var<storage, read> bands: array<u32>;"));
    assert!(w.contains("textureSampleLevel(atlas_tex, atlas_samp, v.uv, 0"));
    assert!(w.contains("pow(cov, gam)"));
    assert!(w.contains("mix(v.color.xyz, u.pulse_ink.xyz, lit)"));

    let mut msl = StackWriter::<32768>::new();
    assert!(emit_slug_site_msl(&mut msl).is_ok());
    let m = msl.as_str();
    assert!(m.contains("#include <metal_stdlib>"));
    assert!(m.contains("uint slug_root_code("));
    assert!(m.contains("vertex slug_site_vs_out slug_site_vs("));
    assert!(m.contains("fragment float4 slug_site_fs("));

    // 2. Build a vector glyph (CCW unit square [0.1..0.9] x [0.0..0.8] in em space) and pack into den buffers!
    let alloc = test_alloc();
    let mut pb = PathBuilder::new_in(alloc);
    pb.move_to([0.1, 0.0]).expect("m");
    pb.line_to([0.9, 0.0]).expect("l1");
    pb.line_to([0.9, 0.8]).expect("l2");
    pb.line_to([0.1, 0.8]).expect("l3");
    let contours = pb.finish().expect("finish");
    let outline = Outline::new(contours, alloc).expect("ok").expect("some");
    let mut curves = OwnVec::new_in(alloc);
    let mut bands = OwnVec::new_in(alloc);
    let meta = pack_outline_into(&outline, &mut curves, &mut bands, alloc).expect("pack");
    assert!(meta.drawable);

    // 3. Evaluate BOTH `slug_render_cpu` AND the ACTUAL Rust GPU fragment shader `slug_site_fs`
    //    at 16 ppem (100% zoom) and 256 ppem (1600% trackpad zoom)!
    let curves_v4: OwnVec<GVec4, _> = {
        let mut v = OwnVec::new_in(alloc);
        for c in curves.as_slice() {
            v.try_push(gvec4(c[0], c[1], c[2], c[3])).expect("push");
        }
        v
    };
    let tex_px = [GVec4::splat(1.0)];
    let tex = gpu::Texture2d::from_rgba32f(1, 1, &tex_px);
    let smp = gpu::Sampler::LINEAR_CLAMP;
    let uniform = SiteUniform {
        view_origin: gvec4(1280.0, 800.0, 0.0, 0.0),
        pulse: gvec4(0.0, 0.0, 1.0, 0.0),
        pulse_ink: gvec4(1.0, 1.0, 1.0, 1.0),
        slug: gvec4(0.0, 1.0, 1.0, 0.25),
    };

    for ppem in [16.0f32, 256.0f32] {
        let cov_in = slug_render_cpu(curves.as_slice(), bands.as_slice(), [0.5, 0.4], meta.band_tf, meta.gdata, [ppem, ppem], 0.0);
        let cov_out = slug_render_cpu(curves.as_slice(), bands.as_slice(), [0.02, 0.4], meta.band_tf, meta.gdata, [ppem, ppem], 0.0);
        assert!(cov_in > 0.99, "interior must be filled at {ppem} ppem (got {cov_in})");
        assert!(cov_out < 0.01, "exterior must be empty at {ppem} ppem (got {cov_out})");

        let v_in = SlugSiteVaryings {
            pos: Position(gvec4(0.0, 0.0, 0.0, 1.0)),
            uv: gvec2(0.5, 0.4),
            local: gvec2(0.0, 0.0),
            color: gpu::Flat(gvec4(1.0, 1.0, 1.0, 1.0)),
            half_ext: gpu::Flat(gvec2(10.0, 10.0)),
            radii: gpu::Flat(gvec4(0.0, 0.0, 0.0, 0.0)),
            fx: gpu::Flat(gvec4(0.0, 0.0, ppem, 0.0)),
            band_tf: gpu::Flat(gvec4(meta.band_tf[0], meta.band_tf[1], meta.band_tf[2], meta.band_tf[3])),
            gdata: gpu::Flat(gpu::prelude::uvec4(meta.gdata[0], meta.gdata[1], meta.gdata[2], meta.gdata[3])),
            ppem: gpu::Flat(ppem),
        };
        let px_in = slug_site_fs(v_in, &uniform, curves_v4.as_slice(), bands.as_slice(), &smp, &tex);
        assert!(px_in.w > 0.99, "real Rust slug_site_fs must match CPU oracle inside glyph at {ppem} ppem");

        let mut v_out = v_in;
        v_out.uv = gvec2(0.02, 0.4);
        let px_out = slug_site_fs(v_out, &uniform, curves_v4.as_slice(), bands.as_slice(), &smp, &tex);
        assert!(px_out.w < 0.01, "real Rust slug_site_fs must match CPU oracle outside glyph at {ppem} ppem");
    }
}
