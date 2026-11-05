//! harness-native-render: what turns a flecs UI world into pixels, shared by the harness app (`harness-native`) and the
//! video renderer (`harness-video`). `draw` is one frame as plain data, `gpu` draws it with Metal (onto a window's
//! drawable or into a texture read back), `text` is den's shaper and glyph atlas, `icons` and `slug` the vector marks,
//! and `paint` walks the draw pass's painted nodes (`harness_native_ui_draw::painted`) into a `Draw`.
pub mod atlas;
pub mod boxdraw;
pub mod draw;
pub mod gpu;
pub mod highlight;
pub mod icons;
pub mod markdown;
pub mod pack;
pub mod paint;
pub mod select;
pub mod slug;
pub mod text;
pub mod vector;
