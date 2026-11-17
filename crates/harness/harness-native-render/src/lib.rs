//! harness-native-render: what turns a flecs UI world into pixels, shared by the harness app (`harness-native`) and the
//! video renderer (`harness-video`). `draw` is one frame as plain data, `gpu` draws it with Metal (onto a window's
//! drawable or into a texture read back), `text` is den's shaper and glyph atlas, `math` den's LaTeX typesetting (RaTeX), `icons` and `slug` the vector marks, `drawing` the multi-colour
//! drawings (the file icons), `kip` den's mascot's Metal pass,
//! `hdr` the EDR headroom and tone-map every picture past white shares,
//! and `paint` walks the draw pass's painted nodes (`harness_native_ui_draw::painted`) into a `Draw`.
pub mod arrive;
pub mod atlas;
pub mod block;
pub mod boxdraw;
pub mod caret;
pub mod draw;
pub mod drawing;
pub mod gpu;
pub mod hdr;
pub mod highlight;
pub mod icons;
pub mod inline;
pub mod kip;
pub mod kips;
pub mod lantern;
pub mod markdown;
pub mod math;
pub mod media;
pub mod pack;
pub mod paint;
pub mod photo;
pub mod select;
#[cfg(test)]
mod shader_port;
pub mod slug;
pub mod text;
pub mod vector;
