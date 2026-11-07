//! The layout-vocabulary gallery (`HARNESS_NATIVE_GALLERY=1` with `HARNESS_NATIVE_SHOT`): the prims crate's example scenes
//! drawn instead of the app's view, in the same App and Renderer. Every other root node is made `Offscreen`; the window's
//! title-bar strip is the gallery root's `SafeAreaInset`; a system keeps the lazy example's rows to the ones in view and,
//! one second in, moves the selection pill, inserts and removes chips, removes a list row and retargets the animated bar, so a shot shortly
//! after (`HARNESS_NATIVE_SHOT_SECS`) catches them mid-spring. `HARNESS_NATIVE_GALLERY=prims` draws the primitives gallery
//! instead (scene/gallery.flecs in the prims crate: one of every primitive, a button, a toggle and a field among them).
use std::time::Instant;

use harness_native_flecs::prelude::*;
use harness_native_ui_layout_api::{LazyRange, Padding, SafeAreaInset, Size, Sizing};
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

/// The generative-UI gallery (`HARNESS_NATIVE_GALLERY=genui`, `HARNESS_NATIVE_GENUI=<tree.json>`): one cell's tree (as the
/// kernel sends it: mcp's `t.ui`) on the terminal's ground, filled each frame by the chat's own `genui::fill`. The root's id.
pub fn load_genui(app: &App, strip: f32) -> Result<u64, String> {
    let w = app.world.world_ref();
    let root = View::root(w, "hn.GenGallery").named("genui_gallery");
    root.entity().set(SafeAreaInset { l: 0.0, r: 0.0, t: strip, b: 0.0 });
    let root_id = root.id();
    w.system_named::<&Node>("GalleryOnly").without(Offscreen::id()).each_entity(move |e, _| {
        if *e.id() != root_id && !e.parent().is_some_and(|p| p.has(Node::id())) {
            e.add(Offscreen::id());
        }
    });
    Ok(root_id)
}

/// The tree `HARNESS_NATIVE_GENUI` names, read.
pub fn genui_tree() -> Result<serde_json::Value, String> {
    let path = std::env::var("HARNESS_NATIVE_GENUI").map_err(|_| "HARNESS_NATIVE_GALLERY=genui needs HARNESS_NATIVE_GENUI=<tree.json>".to_owned())?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("HARNESS_NATIVE_GENUI {path}: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("HARNESS_NATIVE_GENUI {path}: {e}"))
}

/// The cells a CellRef in the gallery's tree shows, when `HARNESS_NATIVE_GENUI_CELLS` names a JSON file of them: an
/// object by tool call id, each `{cell, status, ui, logs, value}` as a transcript's `tool_detail` has them. Read once.
pub fn genui_cells() -> Result<Option<&'static std::collections::HashMap<String, harness_native_model::chat::Detail>>, String> {
    static CELLS: std::sync::OnceLock<Result<Option<std::collections::HashMap<String, harness_native_model::chat::Detail>>, String>> = std::sync::OnceLock::new();
    let read = || -> Result<Option<_>, String> {
        let Ok(path) = std::env::var("HARNESS_NATIVE_GENUI_CELLS") else { return Ok(None) };
        let text = std::fs::read_to_string(&path).map_err(|e| format!("HARNESS_NATIVE_GENUI_CELLS {path}: {e}"))?;
        let all: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("HARNESS_NATIVE_GENUI_CELLS {path}: {e}"))?;
        let serde_json::Value::Object(m) = all else { return Err(format!("HARNESS_NATIVE_GENUI_CELLS {path}: not an object of cells by call")) };
        let word = |d: &serde_json::Value, k: &str| d[k].as_str().unwrap_or("").to_owned();
        Ok(Some(
            m.iter()
                .map(|(call, d)| {
                    let detail = harness_native_model::chat::Detail {
                        cell: word(d, "cell"),
                        status: word(d, "status"),
                        logs: word(d, "logs"),
                        ui: std::sync::Arc::new(d["ui"].clone()),
                        value: std::sync::Arc::new(d["value"].clone()),
                        ..Default::default()
                    };
                    (call.clone(), detail)
                })
                .collect(),
        ))
    };
    CELLS.get_or_init(read).as_ref().map(Option::as_ref).map_err(Clone::clone)
}

/// Load the examples (or, `prims`, the primitives gallery) and make them the only thing drawn; `strip` is the title-bar strip
/// (points) to inset.
pub fn load(app: &App, strip: f32, prims: bool) -> Result<(), String> {
    let world = &app.world;
    world.component::<GalleryStep>();
    let (scenes, root): (&[(&str, &str)], &str) = if prims {
        (core::slice::from_ref(&harness_native_ui_prims::GALLERY), "gallery")
    } else {
        (harness_native_ui_prims::EXAMPLES, ROOT)
    };
    for (name, src) in scenes {
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
    let root = w.try_lookup(root).ok_or_else(|| format!("the gallery scene has no {root}"))?;
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
        table_rows(w);
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

/// The table example: 1 000 rows of made-up search results, an index column, a url column shown as its host, a bool
/// column as marks and a number column at the end (measured by the monospace estimate: a gallery has no typesetter).
fn table_rows(w: WorldRef<'_>) {
    use harness_native_ui_layout::typeset::{Estimate, Measure};
    use view::table::{Align, Cell, Column, Kind, Opts, Table};
    let Some(grid) = at(w, "cards::table::scroll::grid") else { return };
    let sites = ["https://bun.com/docs", "https://en.wikipedia.org/wiki/Rust", "https://sqlite.org/lang.html", "https://docs.rs/flecs"];
    let col = |name: &str, align| Column { name: name.into(), align };
    let data = Table {
        cols: vec![col("#", Align::End), col("title", Align::Auto), col("url", Align::Auto), col("resolved", Align::Auto), col("score", Align::Auto)],
        rows: (0..1_000u32)
            .map(|i| {
                vec![
                    Cell::new(Kind::Dim, i.to_string()),
                    Cell::new(Kind::Text, format!("Result {i}: a title long enough to be cut at the column's edge")),
                    Cell::new(Kind::Url, sites[i as usize % sites.len()]),
                    Cell::new(Kind::Bool, if i % 3 == 0 { "false" } else { "true" }),
                    Cell::new(Kind::Number, format!("{:.1}", f64::from((i * 37) % 100) / 10.0)),
                ]
            })
            .collect(),
        head: true,
    };
    let opts = Opts { look: "ui.Table", shown: data.rows.len(), action: None, open: Some(2), ..Opts::default() };
    view::table(View::of(grid), &data, opts, &mut |s, st, _| Estimate.width(0, s, st));
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
        |_| false,
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
        bar.set(crate::look::tone(w, "accent_wash"));
    }
}
