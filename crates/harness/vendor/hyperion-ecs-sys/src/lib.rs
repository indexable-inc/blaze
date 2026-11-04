//! Raw flecs v4.2.0 bindings (bindgen 0.72.1 with `--use-core`, so pointer-sized C types are `usize` and the file holds on
//! wasm32 too; den-ecs checks the structs it touches against `hyperion_flecs_c::den_flecs_layout`). Declarations only: the C
//! is linked by whoever depends on hyperion-flecs-c (a host binary), so a module dylib built on these resolves `ecs_*` against
//! the host's one flecs. Declarations for addons the linked build leaves out exist here but must not be called.
#![no_std]
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unsafe_op_in_unsafe_fn, clippy::all)]
#![allow(unnecessary_transmutes, unpredictable_function_pointer_comparisons, rustdoc::all)]

mod bindings;
pub use bindings::*;
