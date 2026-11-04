//! harness-native-ui-prims: the primitive prefabs and the one call that sets up the generic UI passes (docs/ui-model.md).
//!
//! The host calls [`register`] with the other components and loads [`SCENES`] after its tokens. The layout, draw and input
//! passes are modules (`harness-native-ui-layout`, `harness-native-ui-draw`, `harness-native-ui-input`) the host installs with its other built-in modules;
//! each frame it writes `Viewport` and `PointerState`, runs [`typeset`] with its typesetter, advances `Paint`, `progress`es,
//! and reads the painted nodes (`harness_native_ui_draw::painted`).
use harness_native_flecs::prelude::*;

pub use harness_native_ui_layout::{Estimate, Measure, typeset};

/// `(name, source)` of the scene scripts, in load order; load after `00-tokens`.
pub const SCENES: &[(&str, &str)] = &[("06-prims", include_str!("../scene/prims.flecs"))];

/// The gallery scene: one of each primitive. Loaded by the gallery test and by a dev command, never by the app's scene.
pub const GALLERY: (&str, &str) = ("90-gallery", include_str!("../scene/gallery.flecs"));

/// Register every api crate's components and relationships, before any scene, query or system uses them.
pub fn register(world: &World) {
    harness_native_ui_node_api::register(world);
    harness_native_ui_layout_api::register(world);
    harness_native_ui_text_api::register(world);
    harness_native_ui_state_api::register(world);
    harness_native_ui_input_api::register(world);
    harness_native_ui_motion_api::register(world);
    harness_native_ui_look_api::register(world);
    harness_native_ui_draw_api::register(world);
}
