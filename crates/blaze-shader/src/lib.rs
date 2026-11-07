//! blaze's device backend for shaders and GPU kernels (docs/future/gpu/shaders.md): MIR to naga's
//! IR, validated by naga and written as MSL, SPIR-V or WGSL by naga's back ends. It is codegen, so
//! it lives here and not in the front end (`semantics-in-front-end`).
//!
//! Built so far: [`structure`], which turns a function's control-flow graph into the structured
//! control flow naga's IR has (`if`, `loop`, `break` and `continue` to the innermost loop, `return`).
//! Next (the plan's phase 2): the MIR to naga lowering over it, the intrinsics table, and the device
//! build kind that roots a crate's build at its `GPU_ENTRIES`.
pub mod structure;
