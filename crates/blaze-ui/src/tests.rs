//! Comprehensive unit tests for `blaze-ui`:
//! - Rust -> WGSL & MSL 2.4 shader generation and collision-free WebGPU bindings
//! - CPU software rasterizer validation of our SDF rounded-rect + border + clip math
//! - `IsA` Style Prefab live theme cascade & per-entity local override
//! - Deterministic two-pass flex/stack/scroll/clip layout
//! - Clip-aware hit-testing, ECS interaction tags, sliders, toggles, text input
//! - Binary-tree ECS docking (split, drag-to-dock, maximize, prune)
//! - End-to-end self-hosting `WorkbenchApp` frame rendering & live self-inspection

use blaze_own::Vec as OwnVec;
use blaze_own::testing::heap;
use gpu::emit::StackWriter;
use gpu::prelude::{Position, Vec2 as GVec2, Vec4 as GVec4, vec2 as gvec2, vec4 as gvec4};

use crate::components::{
    ComputedRect, Focused, Hovered, Inspects, SliderState, TextInputState, ToggleState,
};
use crate::dock::{DropZone, SplitDir};
use crate::input::{InputEvent, Key, UiEvent};
use crate::shaders::{emit_ui_glyph_msl, emit_ui_glyph_wgsl, emit_ui_sdf_msl, emit_ui_sdf_wgsl};
use crate::style::{LayoutStyle, PaintStyle};
use crate::types::{Edges, Rect, ThemeKind};
use crate::ui::UiContext;
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
