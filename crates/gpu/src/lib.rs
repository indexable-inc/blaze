//! Rust on the GPU, the shader side (docs/future/gpu/shaders.md): what a `#[gpu::vertex]` or
//! `#[gpu::fragment]` function is written with, and the binding table its signature generates.
//!
//! - [`vec`]: `Vec2`, `Vec3`, `Vec4`, `UVec4`, `IVec2`, laid out as Metal's `float4` and the rest.
//! - [`math`]: Slang's free functions under Slang's names (`lerp`, `smoothstep`, `saturate`, ...).
//! - [`entry`]: built-ins ([`VertexIndex`], [`InstanceIndex`], [`Position`]), interpolation
//!   ([`Flat`], [`Linear`]) and resources (`&T`, `&[T]`, [`Texture2d`], [`Sampler`]) as types; what
//!   each is, through [`Param`] and [`Varying`]; and [`layout`], the one rule for slots.
//! - [`intrinsic`]: derivatives, sampling and `discard`, lowered by the device backend.
//!
//! Everything here is plain Rust, so a shader crate's functions run on the host in its tests;
//! the intrinsics, which only a GPU has, panic there naming themselves.
// The macros write `::gpu::...`, which this lets the crate's own tests use too.
extern crate self as gpu;

pub mod entry;
pub mod intrinsic;
pub mod math;
pub mod vec;

pub use entry::{
    Code, Entries, Entry, EntryDesc, Field, FieldKind, Flat, InstanceIndex, Interp, Kind, Linear, Out, Output, Param,
    ParamDesc, Pod, Position, Sampler, Slot, Stage, Table, Texture2d, Value, ValueTy, Varying, Varyings, VertexIndex,
    check, layout, layout_fields, sizes_slot,
};
pub use gpu_macros::{Pod, Varyings, fragment, vertex};

/// What a shader module starts with: `use gpu::prelude::*;`.
pub mod prelude {
    pub use crate::entry::{Flat, InstanceIndex, Linear, Position, Sampler, Texture2d, VertexIndex};
    pub use crate::intrinsic::{discard, dpdx, dpdy, fwidth};
    pub use crate::math::{dot, length, lerp, normalize, pow, saturate, smoothstep, step};
    pub use crate::vec::{IVec2, UVec4, Vec2, Vec3, Vec4, ivec2, uvec4, vec2, vec3, vec4};
}

/// The crate's entry points, in one static at the crate root, `GPU_ENTRIES`: what a device build
/// of the crate compiles, found there by its path. Name each `#[gpu::vertex]` or
/// `#[gpu::fragment]` function by its path from the crate root:
///
/// ```ignore
/// gpu::entries![mascot::mascot_vs, mascot::mascot_fs];
/// ```
#[macro_export]
macro_rules! entries {
    ($($($seg:ident)::+),* $(,)?) => {
        /// This crate's GPU entry points (`gpu::entries!`).
        pub static GPU_ENTRIES: $crate::Entries = $crate::Entries(&[
            $($crate::Entry { desc: &$($seg)::+::ENTRY, code: $crate::Code($($seg)::+ as *const ()) }),*
        ]);
    };
}

#[cfg(test)]
mod tests;
