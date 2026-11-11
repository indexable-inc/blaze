//! blaze's device backend for shaders and GPU kernels (docs/future/gpu/shaders.md): MIR to naga's
//! IR, validated by naga and written as MSL, SPIR-V or WGSL by naga's back ends. It is codegen, so
//! it lives here and not in the front end (`semantics-in-front-end`).
//!
//! - [`entries`]: a crate's `GPU_ENTRIES`, read by const evaluation into the entries' interfaces.
//! - [`intrinsics`]: the one table of what lowers by name (the gpu crate's vectors, textures,
//!   derivatives and sampling; core's float and integer intrinsics).
//! - [`structure`]: a reducible control-flow graph as naga's `if`, `loop`, `break` and `continue`.
//! - [`lower`]: an entry's MIR, every call inlined, as one naga entry point.
//! - [`device`]: the device build of a crate: every entry lowered into one module, validated,
//!   and written as MSL with its binding table.
//! - [`refuse`]: what is refused, naming the item and the rule.
pub mod device;
pub mod entries;
pub mod intrinsics;
pub mod lower;
pub mod refuse;
pub mod structure;

#[cfg(test)]
mod tests;
