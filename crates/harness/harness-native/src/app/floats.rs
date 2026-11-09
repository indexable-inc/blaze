//! Windowed overlays (`Windowed`, node-api): the menus, popovers and hover cards that float in a window of their own, so
//! they reach past the main window's edge as NSMenu and NSPopover do. The scene says which overlays are windowed (the
//! overlay prefabs carry the tag); layout places them on the screen the host reports here ([`App::set_screen`]); the draw
//! pass marks what is in each (`Painted::window`), and [`App::emit`](super::App) draws that into a [`Float`] of its own
//! instead of the main window's draw. The host takes the frame's floats ([`App::take_floats`]) and shows each in its window
//! (window.rs and overlay.rs on macOS; a shot lays them over its picture). Geometry and input stay in the main view's
//! points throughout: a float's draw keeps its instances where layout put them and says the part of the view its window
//! shows (`Frame::place` and `view`).
use super::*;
use harness_native_ui_layout_api::Screen;

/// One windowed overlay's draw this frame: the `Windowed` node `id`, the part of the main view its window shows (`rect`,
/// view points: its `Laid`, out to whole device pixels), and what it draws there (`draw`, whose frame's `view` is the
/// rect's size and `place` its corner). `glass`: the node is frosted (`Frost`), so its window has the system's
/// vibrancy behind what it draws, cut to the node's corners (`radius`, view points), where the plate is drawn this frame
/// (`plate`: x, y, w, h from the rect's corner, view points, the node's transition scale applied) and as shown as it is
/// (`alpha`): mid-transition the glass grows, shrinks and fades with the plate, never a frosted shape left standing
/// while what was on it fades away.
pub struct Float {
    pub id: u64,
    pub rect: [f32; 4],
    pub draw: Draw,
    pub glass: bool,
    pub radius: f32,
    pub plate: [f32; 4],
    pub alpha: f32,
}

impl App {
    /// The room windowed overlays are placed and kept in: the visible frame of the screen the window is on, in window
    /// points from the main view's top-left corner (window.rs, each frame; a shot gives its own canvas). Written into the
    /// world as `Screen`, in view points, by the next frame.
    pub fn set_screen(&mut self, r: [f32; 4]) {
        if self.screen != Some(r) {
            self.screen = Some(r);
            self.poke();
        }
    }

    /// The frame's `Screen` at zoom `z`, written when it changed (before layout runs).
    pub(super) fn sync_screen(&mut self, z: f32) {
        let Some(r) = self.screen else { return };
        let s = Screen { x: r[0] / z, y: r[1] / z, w: r[2] / z, h: r[3] / z };
        if self.world.world_ref().try_get::<&Screen>(|v| *v) != Some(s) {
            self.world.set(s);
        }
    }

    /// The windowed overlays the last frame drew, each with its draw (none: no overlay's window is shown).
    pub fn take_floats(&mut self) -> Vec<Float> {
        std::mem::take(&mut self.floats)
    }

    /// The window lost the keyboard (another app or window took it): what floats lightly closes, as NSMenu and NSPopover
    /// do (the picker's menus, a sidebar row's menu, a cell's popovers and menus; a cell's dialog stays, as a sheet
    /// would). Whether any closed.
    pub fn close_floats(&mut self) -> bool {
        let pick = self.chat_pick_close();
        let row = self.row_menu_close();
        let genui = self.genui_blur();
        pick || row || genui
    }
}

/// The draw a painted node of window `window` goes into this frame: the main one for 0, else that float's (made on its
/// first node, with the main frame's look).
pub(super) fn target<'d>(main: &'d mut Draw, floats: &'d mut Vec<(u64, Draw)>, window: u64) -> &'d mut Draw {
    if window == 0 {
        return main;
    }
    let at = match floats.iter().position(|f| f.0 == window) {
        Some(i) => i,
        None => {
            floats.push((window, Draw { frame: main.frame, clear: [0.0; 4], ..Draw::default() }));
            floats.len() - 1
        }
    };
    &mut floats[at].1
}

/// The floats' draws as their windows show them: each over its `Windowed` node's `Laid`, out to whole device pixels at
/// scale `sc` (so a plate snapped to the main view's pixels lands on the window's), its frame's `view` that rect's size
/// and `place` its corner. A float whose node has no place (deleted mid-frame) is dropped with an error naming it.
pub(super) fn finish(w: WorldRef<'_>, floats: Vec<(u64, Draw)>, sc: f32) -> Vec<Float> {
    let mut out = Vec::with_capacity(floats.len()); // alloc: the frame's windowed overlays, none or one or two
    for (id, mut draw) in floats {
        let Some(l) = w.entity_from_id(id).try_cloned::<&Laid>() else {
            eprintln!("harness-native: windowed overlay {id} was drawn but has no Laid: its window is not shown this frame");
            continue;
        };
        let (x0, y0) = ((l.x * sc).floor() / sc, (l.y * sc).floor() / sc);
        let (x1, y1) = (((l.x + l.w) * sc).ceil() / sc, ((l.y + l.h) * sc).ceil() / sc);
        let rect = [x0, y0, (x1 - x0).max(1.0 / sc), (y1 - y0).max(1.0 / sc)];
        draw.frame.view = [rect[2], rect[3]];
        draw.frame.place = [rect[0], rect[1], 0.0, 0.0];
        let node = w.entity_from_id(id);
        let glass = node.has(harness_native_ui_look_api::Frost::id());
        // Where the draw pass put the plate this frame (moved and scaled by its transition) and how shown it is.
        let painted = node.try_cloned::<&harness_native_ui_draw_api::Painted>();
        let (plate, alpha, scale) = painted.map_or(([l.x - rect[0], l.y - rect[1], l.w, l.h], 1.0, 1.0), |p| {
            ([p.x - rect[0], p.y - rect[1], p.w * p.scale, p.h * p.scale], p.alpha.clamp(0.0, 1.0), p.scale)
        });
        let radius = node.try_cloned::<&harness_native_ui_look_api::Shape>().map_or(0.0, |s| s.radius) * scale;
        out.push(Float { id, rect, draw, glass, radius, plate, alpha });
    }
    out
}
