//! WHAT THE CAD LIBRARY HANDS TO THE WORLD (docs/v2/plan/design/end-state.md, D183): parts as a
//! binary glTF, verified solids as binary STL, 2D cross-sections as vector SVG, images as PNG,
//! and a one-line-per-part summary for a person or an agent.
//!
//! | module | what |
//! |---|---|
//! | [`scene`] | [`Scene`], [`Part`], [`Material`], [`MaterialTextures`], [`TexturePack`] and what a part carries: [`Transform`], [`MassFacts`], [`Joint`], [`Collider`], [`Lod`] |
//! | [`glb`] | [`glb()`], [`glb_textured()`]: glTF 2.0 in one `.glb`, float or [`KHR_mesh_quantization`](Encoding::Quantized), UVs, tangents, embedded PNG textures |
//! | [`glb_read`] | [`read_glb()`]: binary glTF 2.0 reader reconstructing meshes, hierarchy, PBR materials, and decoded PNG images |
//! | [`stl`] | [`stl()`]: standard binary STL from a verified closed manifold mesh, in mm or m |
//! | [`svg`] | [`svg()`]: standalone SVG 1.1 2D cross-section via Marching Squares with saddle decider |
//! | [`png`] | [`png()`], [`decode_png()`]: an RGBA8 or 8-bit grey PNG encoder and decoder, zlib on blaze-deflate, CRC-32 per chunk |
//! | [`summary`] | [`summary()`]: one part per line, plain text |
//! | `json` | the JSON emitter the glTF's JSON chunk is written with, on blaze-text |
//!
//! # The same bytes everywhere
//!
//! Every output is a function of its input alone (rule 25): keys in a fixed order, numbers in
//! the shortest form that reads back as the same `f64` (blaze-text's writer, never `core::fmt`),
//! no hash table iterated, nothing read from a clock, the environment or a thread. Two exports
//! of one scene are byte-identical, and the tests hold one single-triangle `.glb` byte for byte.
//!
//! # Memory
//!
//! Every function that allocates takes the allocator of its output (`alloc`) and of its scratch
//! (`scratch`) and returns a typed error naming a refusal; nothing allocates through `Global`.

#![no_std]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod glb;
pub mod glb_read;
mod json;
mod plant;
pub mod png;
pub mod scene;
mod sink;
pub mod stl;
pub mod summary;
pub mod svg;

#[cfg(test)]
mod tests;

pub use glb::{
    Encoding, Field, GlbError, GlbOpts, MaterialField, MeshSlot, NormalBits, glb, glb_planted, glb_textured,
    glb_textured_planted,
};
pub use glb_read::{GlbReadError, GlbReadOpts, LoadedGlb, LoadedImage, LoadedLod, LoadedMaterial, LoadedPart, read_glb};
pub use json::JsonFault;
pub use plant::{GlbPlant, PngPlant, StlPlant, SummaryPlant, SvgPlant};
pub use png::{DecodedPng, Image, PngError, PngKind, PngOpts, decode_png, png, png_planted};
pub use scene::{
    Clearcoat, Collider, Joint, JointKind, Lod, MassFacts, Material, MaterialTextures, Matrix, Part, Scene, TexturePack,
    Transform, Trs, Up,
};
pub use stl::{FACET_BYTES, HEADER_BYTES, StlError, StlOpts, StlUnit, stl, stl_planted};
pub use summary::{SummaryError, SummaryOpts, summary, summary_planted};
pub use svg::{DEFAULT_FILL, DEFAULT_STROKE, SectionGrid, SvgError, SvgOpts, svg, svg_planted};

/// The caps of every writer here (D165: a field each, the shipped values one constant).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// The most parts a scene may have.
    pub most_parts: u32,
    /// The most bytes a `.glb` may be: at most `u32::MAX`, the format's own length field.
    pub most_glb_bytes: u32,
    /// The most pixels an image may have.
    pub most_pixels: u64,
    /// The most bytes a binary `.stl` may be.
    pub most_stl_bytes: u64,
    /// The most grid cells `(nx - 1) · (ny - 1)` an `.svg` section may contour.
    pub most_svg_cells: u64,
}

impl Limits {
    /// What blaze ships: a million parts, the format's own 4 GiB for `.glb` and `.stl`,
    /// 2^28 pixels (16,384 × 16,384) for PNG, and 2^24 cells (4,096 × 4,096) for SVG sections.
    pub const SHIPPED: Limits = Limits {
        most_parts: 1 << 20,
        most_glb_bytes: u32::MAX,
        most_pixels: 1 << 28,
        most_stl_bytes: 1 << 32,
        most_svg_cells: 1 << 24,
    };
}
