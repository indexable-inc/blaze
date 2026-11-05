//! The system pointer's shape, macOS's conventions: the I-beam over text to select or type into, the column-resize pointer
//! over the sidebar's draggable edge and the whole time it is dragged, the arrow over controls (buttons, rows, chips) and
//! everywhere else. Resolved once a frame (`resolve_pointer`, at the end of `App::frame`); window.rs sets NSCursor only when
//! the shape changes and again on AppKit's `cursorUpdate:`.
//!
//! What decides, first to last:
//! 1. A drag in progress: the sidebar's edge (column resize, one-way at its width's limits), a terminal selection (I-beam),
//!    a press reported to a terminal's program (arrow), a press on a node (the shape it went down with).
//! 2. The sidebar's draggable edge zone (the same test a press uses, `on_side_edge`).
//! 3. The topmost hit node under the pointer: its `Cursor`, or its nearest ancestor's, said on the primitives and prefabs in
//!    script (prims.flecs: `ui.Field` is Text) and resolved by the input pass into `PointerShape`
//!    (harness-native-ui-input); a node that says none is the arrow.
//! 4. The title-bar strip: the arrow (AppKit's own).
//! 5. A terminal's grid (host-owned, its cells are not nodes): the I-beam, or the arrow while a press there would go to the
//!    program (src/app/panes.rs `term_zone_pointer`).
//! 6. The arrow.
//!
//! `HARNESS_NATIVE_CURSOR_LOG=1` prints each change; F3's overlay shows the shape and why.
use super::*;
use harness_native_ui_input_api::PointerShape;

impl App {
    /// Whether the pointer is on the sidebar's draggable right edge, where a press drags its width: the sidebar shown,
    /// no palette over it, within `SIDE_GRAB` points of the edge along its height.
    pub(super) fn on_side_edge(&self) -> bool {
        if self.palette.is_some() || !self.side_open {
            return false;
        }
        let Some(l) = self.sidebar().and_then(|s| s.try_cloned::<&Laid>()).filter(|l| l.w > 0.0) else {
            return false;
        };
        let (x, y) = (self.pointer.x, self.pointer.y);
        (x - (l.x + l.w)).abs() <= SIDE_GRAB && y >= l.y && y <= l.y + l.h
    }

    /// The sidebar edge's pointer: both ways, or only the way its width can still go at `SIDE_MIN` or `SIDE_MAX`
    /// (NSSplitView's divider at its limit).
    fn side_edge_pointer(&self) -> Pointer {
        let w = self.side_w.or_else(|| self.sidebar().and_then(|s| s.try_cloned::<&Laid>()).map(|l| l.w));
        match w {
            Some(w) if w <= SIDE_MIN => Pointer::ResizeRight,
            Some(w) if w >= SIDE_MAX => Pointer::ResizeLeft,
            _ => Pointer::ResizeLeftRight,
        }
    }

    /// The system pointer's shape now, and why (the module's doc has the order).
    pub fn pointer_shape(&self) -> (Pointer, &'static str) {
        if self.resizing {
            return (self.side_edge_pointer(), "drag: sidebar edge");
        }
        if let Some(s) = self.term_drag_pointer() {
            return s;
        }
        let scene = self.world.try_get::<&PointerShape>(|s| *s).unwrap_or_default();
        if self.pointer.down && scene.node {
            return (scene.shape, "press: node");
        }
        // A press on no node holds no edge: only a hover finds it.
        if !self.pointer.down && self.on_side_edge() {
            return (self.side_edge_pointer(), "sidebar edge");
        }
        if scene.node {
            return (scene.shape, "node");
        }
        if self.chrome.strip > 0.0 && self.pointer.y >= 0.0 && self.pointer.y < self.chrome.strip / self.zoom() {
            return (Pointer::Arrow, "title bar");
        }
        if let Some(s) = self.term_zone_pointer() {
            return s;
        }
        (Pointer::Arrow, "none")
    }

    /// The pointer's shape as last resolved (window.rs sets the system pointer to it).
    pub fn shape(&self) -> Pointer {
        self.shape.0
    }

    /// The pointer's shape and why, for F3's overlay and the shot's log (`ibeam  terminal`).
    pub fn pointer_line(&self) -> String {
        format!("{}  {}", pointer_name(self.shape.0), self.shape.1)
    }

    /// Resolve the pointer's shape for this frame, logging a change under `HARNESS_NATIVE_CURSOR_LOG`.
    pub(super) fn resolve_pointer(&mut self) {
        let now = self.pointer_shape();
        if now != self.shape {
            self.shape = now;
            if std::env::var_os("HARNESS_NATIVE_CURSOR_LOG").is_some() {
                eprintln!(
                    "harness-native: pointer {} at {:.0},{:.0}",
                    self.pointer_line(),
                    self.pointer.x,
                    self.pointer.y
                );
            }
        }
    }

    /// The modifier keys changed with no pointer event (Shift over a terminal that reports the mouse turns the pointer to
    /// the I-beam).
    pub fn set_mods(&mut self, m: KeyMods) {
        if m != self.mods {
            self.mods = m;
            self.poke();
        }
    }
}

/// A shape's name as macOS's NSCursor calls it, lower case.
pub fn pointer_name(p: Pointer) -> &'static str {
    match p {
        Pointer::Arrow => "arrow",
        Pointer::Text => "ibeam",
        Pointer::VerticalText => "ibeam-vertical",
        Pointer::Hand => "pointing-hand",
        Pointer::OpenHand => "open-hand",
        Pointer::Grabbing => "closed-hand",
        Pointer::ResizeLeftRight => "column-resize",
        Pointer::ResizeLeft => "column-resize-left",
        Pointer::ResizeRight => "column-resize-right",
        Pointer::ResizeUpDown => "row-resize",
        Pointer::NotAllowed => "not-allowed",
    }
}
