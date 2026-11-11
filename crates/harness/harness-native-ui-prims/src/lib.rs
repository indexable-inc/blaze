//! harness-native-ui-prims: the primitive prefabs and the one call that sets up the generic UI passes (docs/ui-model.md).
//!
//! The host calls [`register`] with the other components and loads [`SCENES`] after its tokens. The layout, draw and input
//! passes are modules (`harness-native-ui-layout`, `harness-native-ui-draw`, `harness-native-ui-input`) the host installs with its other built-in modules;
//! each frame it writes `Viewport` and `PointerState`, runs [`typeset`] with its typesetter, advances `Paint`, `progress`es,
//! and reads the painted nodes (`harness_native_ui_draw::painted`).
use harness_native_flecs::prelude::*;

pub use harness_native_ui_layout::{Estimate, Measure, typeset};

/// `(name, source)` of the scene scripts, in load order; load after `00-tokens`.
pub const SCENES: &[(&str, &str)] = &[
    ("06-prims", include_str!("../scene/prims.flecs")),
    ("06-search-menu", include_str!("../scene/search_menu.flecs")),
    ("06-level", include_str!("../scene/level.flecs")),
    ("07-kip", include_str!("../scene/kip.flecs")),
    ("08-field", include_str!("../scene/field.flecs")),
    // The `ui.inline` module: inline components' vocabulary (harness-native-ui-inline is its grammar).
    ("08-inline", include_str!("../scene/inline.flecs")),
    // The `cue.*` moves a timeline makes and the `voice.*` voices a sound is made in (harness-native-ui-timeline-api).
    ("09-cue", include_str!("../scene/cue.flecs")),
    ("09-voice", include_str!("../scene/voice.flecs")),
];

/// The gallery scene: one of each primitive. Loaded by the gallery test and by a dev command, never by the app's scene.
pub const GALLERY: (&str, &str) = ("90-gallery", include_str!("../scene/gallery.flecs"));

/// The SwiftUI-vocabulary examples, one scene per feature (grid, lazy, alignment, truncation, composition and safe area,
/// motion, bubble), then the gallery that lays them all out (`swiftui_gallery`). Loaded after [`SCENES`] by the host's gallery mode
/// (`HARNESS_NATIVE_GALLERY=1`), never by the app's scene.
pub const EXAMPLES: &[(&str, &str)] = &[
    ("91-ex-card", include_str!("../scene/examples/00-card.flecs")),
    ("92-ex-grid", include_str!("../scene/examples/grid.flecs")),
    ("93-ex-lazy", include_str!("../scene/examples/lazy.flecs")),
    ("94-ex-align", include_str!("../scene/examples/align.flecs")),
    ("95-ex-truncate", include_str!("../scene/examples/truncate.flecs")),
    ("96-ex-compose", include_str!("../scene/examples/compose.flecs")),
    ("97-ex-motion", include_str!("../scene/examples/motion.flecs")),
    ("98-ex-stack", include_str!("../scene/examples/stack.flecs")),
    ("98-ex-table", include_str!("../scene/examples/table.flecs")),
    ("98-ex-bubble", include_str!("../scene/examples/bubble.flecs")),
    ("98-ex-scroller", include_str!("../scene/examples/scroller.flecs")),
    ("98-ex-menu", include_str!("../scene/examples/menu.flecs")),
    ("98-ex-search-menu", include_str!("../scene/examples/search_menu.flecs")),
    ("98-ex-level", include_str!("../scene/examples/level.flecs")),
    ("98-ex-browser", include_str!("../scene/examples/browser.flecs")),
    ("98-ex-art", include_str!("../scene/examples/art.flecs")),
    ("98-ex-sheen", include_str!("../scene/examples/sheen.flecs")),
    ("98-ex-block", include_str!("../scene/examples/block.flecs")),
    ("99-ex-gallery", include_str!("../scene/examples/99-gallery.flecs")),
];

/// Register every api crate's components and relationships, before any scene, query or system uses them.
pub fn register(world: &World) {
    harness_native_ui_node_api::register(world);
    harness_native_ui_layout_api::register(world);
    harness_native_ui_text_api::register(world);
    harness_native_ui_state_api::register(world);
    harness_native_ui_browser_api::register(world);
    harness_native_ui_input_api::register(world);
    harness_native_ui_motion_api::register(world);
    harness_native_ui_look_api::register(world);
    harness_native_ui_draw_api::register(world);
    harness_native_ui_timeline_api::register(world);
}
