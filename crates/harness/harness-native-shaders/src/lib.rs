//! The harness renderer's shaders in Rust, compiled by blaze (docs/future/gpu/shaders.md). Each
//! module is one Slang file of harness-native-render's shaders/, ported function for function under
//! the same names and in the same order, so the two read side by side until the Slang one goes.
//!
//! The host depends on this crate for the types its buffers hold and for each entry point's
//! binding table (`harness::panel_fs::insts` is where `panel_fs` binds `insts`); a device build of it
//! (`artifact = "gpu"`) is the shaders themselves. Which shaders the host takes from here rather
//! than from Slang is harness-native-render's build.rs's `SLANG` list.
pub mod harness;
pub mod mascot;

gpu::entries![
    harness::panel_vs,
    harness::panel_fs,
    harness::line_vs,
    harness::line_fs,
    harness::path_vs,
    harness::path_fs,
    harness::image_vs,
    harness::image_fs,
    harness::surface_vs,
    harness::surface_fs,
    harness::sprite_vs,
    harness::sprite_fs,
    harness::glyph_vs,
    harness::glyph_fs,
    mascot::mascot_vs,
    mascot::mascot_fs,
    mascot::mascot_quad_vs,
    mascot::mascot_quad_fs,
];

#[cfg(test)]
mod harness_tests;
#[cfg(test)]
mod tests;
