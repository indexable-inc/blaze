//! Rust on the GPU, the shader side (docs/future/gpu/shaders.md): strictly `#![no_std]` with
//! zero `crates.io` dependencies and zero implicit heap allocations.
//!
//! - [`vec`]: `Vec2`, `Vec3`, `Vec4`, `UVec2`, `UVec3`, `UVec4`, `IVec2`, `IVec3`, `IVec4`, laid
//!   out as Metal's `float4` and the rest, with full integer bitwise ops.
//! - [`mat`]: column-major `#[repr(C)]` `Mat2`, `Mat3`, `Mat4` (`Pod`).
//! - [`math`]: Slang's free functions and deterministic IEEE-754 `#![no_std]` math (`lerp`,
//!   `smoothstep`, `saturate`, `frac`, `fract`, `sign`, `pow`, `exp`, `exp2`, `ln`, `log2`, `sin`,
//!   `cos`, `tan`, `atan2`, `sqrt`, `rsqrt`, `fma`, `cross`, `distance`, `reflect`, `refract`,
//!   `faceforward`, `asuint`, `asint`, `asfloat`, `f32tof16`, `f16tof32`).
//! - [`entry`]: built-ins ([`VertexIndex`], [`InstanceIndex`], [`Position`], [`Thread`]),
//!   interpolation ([`Flat`], [`Linear`]) and resources (`&T`, `&[T]`, [`RawOut`], [`Texture2d`],
//!   [`Sampler`], [`Tlas`]) as types; [`layout`] for Metal's 3-table [`Slot`] and
//!   [`EntryDesc::wgsl_binding`] for flat Vulkan/WGSL `(group, binding)` layouts.
//! - [`compute`]: safe disjoint output views ([`compute::Out`], [`Chunks`], [`Tiles`]), dispatch
//!   verification ([`Grid`], [`LaunchProof`], [`check_launch`]), and phased workgroup shared memory
//!   ([`SharedMem`], [`SharedWrite`] → `.barrier()` → [`SharedRead`]).
//! - [`ray`]: inline ray queries ([`RayQuery`], [`RayFlags`], [`Candidate`], [`Tlas::trace`],
//!   [`Tlas::occluded`]) executing Möller–Trumbore ray-triangle & procedural AABB intersection on CPU.
//! - [`cpu`]: deterministic `#![no_std]` CPU compute dispatcher, phased workgroup runner, 2×2
//!   fragment quad evaluator (`dpdx`, `dpdy`, `fwidth`, `discard`), and software triangle rasterizer.
//! - [`emit`]: zero-dep `#![no_std]` structured Shader IR → MSL 2.4 & WGSL emitter driven by
//!   [`EntryDesc`].

#![cfg_attr(not(test), no_std)]
#![feature(core_float_math)]

// The macros write `::gpu::...`, which this lets the crate's own tests use too.
#[cfg(test)]
extern crate self as gpu;

pub mod compute;
pub mod cpu;
pub mod emit;
pub mod entry;
pub mod intrinsic;
pub mod mat;
pub mod math;
pub mod ray;
pub mod vec;

pub use compute::{
    Chunks, Grid, LaunchError, LaunchProof, SharedMem, SharedRead, SharedWrite, Tiles, check_launch,
    check_launch_grid, workgroup_barrier,
};
pub use entry::{
    AddressMode, Code, Entries, Entry, EntryDesc, Field, FieldKind, Filter, Flat, HostTexture2d, InstanceIndex,
    Interp, Kind, Linear, MAX_BUFFERS, MAX_SAMPLERS, MAX_TEXTURES, MAX_THREADS, Out, Output, Param, ParamDesc, Pod,
    Position, RawOut, Sampler, Slot, Stage, Table, Texture2d, Thread, Value, ValueTy, Varying, Varyings, VertexIndex,
    check, layout, layout_fields, mip_chain_texels, sizes_slot,
};
pub use gpu_macros::{Pod, Varyings, fragment, kernel, vertex};
pub use mat::{Mat2, Mat3, Mat4};
pub use ray::{Candidate, Hit, HostTlas, ProceduralAabb, Ray, RayFlags, RayQuery, Tlas, Triangle};

/// What a shader module starts with: `use gpu::prelude::*;`.
pub mod prelude {
    pub use crate::compute::{Chunks, Grid, SharedMem, SharedRead, SharedWrite, Tiles};
    pub use crate::entry::{
        AddressMode, Filter, Flat, InstanceIndex, Linear, Position, RawOut, Sampler, Texture2d, Thread, VertexIndex,
    };
    pub use crate::intrinsic::{discard, dpdx, dpdy, fwidth};
    pub use crate::mat::{Mat2, Mat3, Mat4};
    pub use crate::math::{
        abs, asfloat, asint, asuint, atan2, ceil, cos, cross, distance, dot, exp, exp2, f16tof32, f32tof16,
        faceforward, floor, fma, frac, fract, length, lerp, ln, log2, normalize, pow, reflect, refract, round, rsqrt,
        saturate, sign, sin, smoothstep, sqrt, step, tan, trunc,
    };
    pub use crate::ray::{Candidate, Hit, ProceduralAabb, Ray, RayFlags, RayQuery, Tlas, Triangle};
    pub use crate::vec::{
        IVec2, IVec3, IVec4, UVec2, UVec3, UVec4, Vec2, Vec3, Vec4, ivec2, ivec3, ivec4, uvec2, uvec3, uvec4, vec2,
        vec3, vec4,
    };
}

/// The crate's entry points, in one static at the crate root, `GPU_ENTRIES`: what a device build
/// of the crate compiles, found there by its path. Name each `#[gpu::vertex]`,
/// `#[gpu::fragment]` or `#[gpu::kernel]` function by its path from the crate root:
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
