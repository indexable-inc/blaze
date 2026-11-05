//! The layout-vocabulary gallery (`HARNESS_NATIVE_GALLERY=1` with `HARNESS_NATIVE_SHOT`): the prims crate's example scenes
//! drawn instead of the app's view, in the same App and Renderer. Every other root node is made `Offscreen`; the window's
//! title-bar strip is the gallery root's `SafeAreaInset`; a system keeps the lazy example's rows to the ones in view and,
//! one second in, moves the selection pill, inserts and removes chips, removes a list row and retargets the animated bar, so a shot shortly
//! after (`HARNESS_NATIVE_SHOT_SECS`) catches them mid-spring.
use std::time::Instant;

use harness_native_flecs::prelude::*;
use harness_native_ui_layout_api::{LazyRange, Padding, SafeAreaInset, Size, Sizing};
use harness_native_ui_look_api::{Fill, Rgba};
use harness_native_ui_node_api::{Node, Offscreen};
use harness_native_ui_view::{self as view, View};

use crate::app::App;

/// The gallery's script clock: whether the one-second changes were made.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct GalleryStep {
    pub changed: bool,
}

const ROOT: &str = "swiftui_gallery";

fn at<'a>(w: WorldRef<'a>, path: &str) -> Option<EntityView<'a>> {
    w.try_lookup(&format!("{ROOT}::{path}"))
}

/// Load the examples and make them the only thing drawn; `strip` is the title-bar strip (points) to inset.
pub fn load(app: &App, strip: f32) -> Result<(), String> {
    let world = &app.world;
    world.component::<GalleryStep>();
    for (name, src) in harness_native_ui_prims::EXAMPLES {
        let e = world.script_named(&format!("scene::{name}")).build_from_code(src);
        let err = e.try_get::<&flecs::Script>(|s| {
            // SAFETY: `error` is null or a NUL-terminated string owned by the EcsScript component, alive for this call.
            (!s.error.is_null()).then(|| {
                unsafe { std::ffi::CStr::from_ptr(s.error) }.to_string_lossy().into_owned()
            })
        });
        if let Some(Some(err)) = err {
            return Err(format!("gallery scene {name}: {err}"));
        }
    }
    let w = world.world_ref();
    let root = w.try_lookup(ROOT).ok_or("the gallery scene has no swiftui_gallery")?;
    root.set(SafeAreaInset { l: 0.0, r: 0.0, t: strip, b: 0.0 });
    root.set(GalleryStep::default());
    let root_id = *root.id();
    // Only the gallery draws: every other root (the window, the title bar, a palette) is out of the frame.
    w.system_named::<&Node>("GalleryOnly").without(Offscreen::id()).each_entity(move |e, _| {
        if *e.id() != root_id && !e.parent().is_some_and(|p| p.has(Node::id())) {
            e.add(Offscreen::id());
        }
    });
    // Wall time from load, the same clock the shot's HARNESS_NATIVE_SHOT_SECS counts.
    let start = Instant::now();
    let mut heights = view::Heights::default();
    w.system_named::<&mut GalleryStep>("GalleryDrive").each_iter(move |it, _, step| {
        let w = it.world();
        lazy_rows(w);
        stack_rows(w, &mut heights);
        let time = start.elapsed().as_secs_f32();
        label_chips(w);
        if time >= 1.0 && !step.changed {
            step.changed = true;
            change(w);
        }
    });
    Ok(())
}

/// The lazy example's rows: only those in view, and a line saying how many exist.
fn lazy_rows(w: WorldRef<'_>) {
    let Some(rows) = at(w, "cards::lazy::scroll::rows") else { return };
    let live = view::lazy_for_each(
        View::of(rows),
        "ui.Words",
        10_000,
        |i| format!("r{i}"),
        |v, i, _| {
            v.text(&format!("Row {i}  \u{b7}  session s{}", (i * 7) % 997));
        },
    );
    let r = rows.try_cloned::<&LazyRange>().unwrap_or_default();
    if let Some(status) = at(w, "cards::lazy::status") {
        View::of(status).text(&format!("{live} of 10 000 exist (rows {}..{})", r.first, r.last));
    }
}

/// The inserted chips' words, once their parts exist (a chip made in a system is instantiated when the system ends).
/// The lazy stack's rows: every third one wraps to a few lines, so rows are of different heights.
fn stack_rows(w: WorldRef<'_>, heights: &mut view::Heights) {
    let Some(rows) = at(w, "cards::stack::scroll::rows") else { return };
    let room = at(w, "cards::stack::scroll")
        .and_then(|s| s.try_cloned::<&harness_native_ui_node_api::Laid>())
        .map_or(132.0, |l| l.h);
    let mut live = 0;
    let s = view::lazy_stack(
        View::of(rows),
        heights,
        0.0,
        room,
        2_000,
        |i| format!("r{i}"),
        |_| "ui.Words",
        |v, i, _| {
            live += 1;
            let long = if i % 3 == 0 { " that runs on and on until it wraps onto a second line and maybe a third" } else { "" };
            v.text(&format!("Row {i}{long}"));
        },
    );
    if let Some(status) = at(w, "cards::stack::status") {
        View::of(status).text(&format!("{live} of 2 000 exist, {:.0} points in all", s.content));
    }
}

fn label_chips(w: WorldRef<'_>) {
    let Some(chips) = at(w, "cards::transition::chips") else { return };
    for (name, words) in
        [("c_fade", "Opacity"), ("c_slide", "Slide"), ("c_scale", "Scale"), ("c_move", "Move")]
    {
        if let Some(words_view) = View::of(chips).part(name).and_then(|c| c.part("words")) {
            words_view.text(words);
        }
    }
}

/// The one-second changes: the pill to the third row, chips in and out, a list row removed, the bar retargeted.
fn change(w: WorldRef<'_>) {
    if let Some(pill) = at(w, "cards::matched::rows::r0::pill") {
        pill.destruct();
    }
    if let Some(r2) = at(w, "cards::matched::rows::r2") {
        View::of(r2).child("ex.Selection").named("pill");
    }
    if let Some(chips) = at(w, "cards::transition::chips") {
        let chips = View::of(chips);
        if let Some(first) = chips.part("first") {
            view::leave(first);
        }
        for (name, prefab) in [
            ("c_fade", "ex.Chip"),
            ("c_slide", "ex.ChipSlide"),
            ("c_scale", "ex.ChipScale"),
            ("c_move", "ex.ChipMove"),
        ] {
            chips.child(prefab).named(name);
        }
    }
    if let Some(b) = at(w, "cards::removal::rows::b") {
        view::leave(View::of(b));
    }
    if let Some(bar) = at(w, "cards::animate::bar") {
        bar.set(Size { w: Sizing::fixed(240.0), h: Sizing::fixed(40.0) });
        bar.set(Padding { l: 40.0, r: 6.0, t: 0.0, b: 0.0 });
        let accent = Rgba { r: 0.58, g: 0.6, b: 1.0, a: 0.45 };
        bar.set(Fill { dark: accent, light: Rgba { r: 0.37, g: 0.38, b: 0.9, a: 0.35 } });
    }
}
