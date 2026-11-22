//! WHAT THE CAD LIBRARY HANDS TO THE WORLD (docs/v2/plan/design/end-state.md, D183): parts as a
//! binary glTF, images as PNG, and a one-line-per-part summary for a person or an agent.
//!
//! | module | what |
//! |---|---|
//! | [`scene`] | [`Scene`], [`Part`], [`Material`] and what a part carries: [`Transform`], [`MassFacts`], [`Joint`], [`Collider`], [`Lod`] |
//! | [`glb`] | [`glb()`]: glTF 2.0 in one `.glb`, float or [`KHR_mesh_quantization`](Encoding::Quantized) |
//! | [`png`] | [`png()`]: an RGBA8 or 8-bit grey PNG, filtered per row, zlib on blaze-deflate, CRC-32 per chunk |
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
mod json;
mod plant;
pub mod png;
pub mod scene;
mod sink;
pub mod summary;

#[cfg(test)]
mod tests;

pub use glb::{Encoding, Field, GlbError, GlbOpts, MaterialField, MeshSlot, NormalBits, glb, glb_planted};
pub use json::JsonFault;
pub use plant::{GlbPlant, PngPlant, SummaryPlant};
pub use png::{Image, PngError, PngKind, PngOpts, png, png_planted};
pub use scene::{Clearcoat, Collider, Joint, JointKind, Lod, MassFacts, Material, Matrix, Part, Scene, Transform, Trs, Up};
pub use summary::{SummaryError, SummaryOpts, summary, summary_planted};

/// The caps of every writer here (D165: a field each, the shipped values one constant).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// The most parts a scene may have.
    pub most_parts: u32,
    /// The most bytes a `.glb` may be: at most `u32::MAX`, the format's own length field.
    pub most_glb_bytes: u32,
    /// The most pixels an image may have.
    pub most_pixels: u64,
}

impl Limits {
    /// What blaze ships: a million parts, the format's own 4 GiB, and 2^28 pixels (16,384 ×
    /// 16,384).
    pub const SHIPPED: Limits = Limits { most_parts: 1 << 20, most_glb_bytes: u32::MAX, most_pixels: 1 << 28 };
}
